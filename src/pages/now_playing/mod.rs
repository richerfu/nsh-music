//! Full-window / full-screen immersive Now Playing stage aligned 1:1 with
//! `egoist/mygo-music` (`internal/app/ui_nowplaying.go`) across Desktop,
//! Pad (Landscape & Portrait), and Phone form factors.

pub mod backdrop;
pub mod lyrics_view;
pub mod panels;
pub mod retro;
pub mod stage;
pub mod wall;

use nsh_base::{SliderValue, button, slider};
use nsh_core::element::desktop::title_bar;
use nsh_core::prelude::*;
use nsh_music_audio::PlaybackStatus;
use nsh_music_core::model::{Quality, RepeatMode, Track};

use crate::{
    state::{AppStore, NowPlayingMode, NowPlayingPanel, Route},
    ui::{
        ContextMenuAction,
        icons::{Icon, icon, icon_reactive},
        loading_spinner, motion,
        scrubber::scrubber,
        styled_lazy_menu,
        theme::{self, DeviceForm, Elevated, Elevation, colors},
        track_row::format_duration_ms,
    },
};

/// Max combined width of the Desktop cover + right pane (`npMaxWidth = 1280`).
const NP_MAX_WIDTH: f32 = 1280.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhoneSection {
    Art,
    Lyrics,
    Stage,
    More,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhoneMoreSubpanel {
    Queue,
    Wall,
    Comments,
    Podcast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageLayoutKind {
    Stage,
    Wall,
    Split,
}

fn stage_layout_kind(panel: NowPlayingPanel) -> StageLayoutKind {
    match panel {
        NowPlayingPanel::Stage => StageLayoutKind::Stage,
        NowPlayingPanel::Wall => StageLayoutKind::Wall,
        NowPlayingPanel::Lyrics
        | NowPlayingPanel::Queue
        | NowPlayingPanel::Comments
        | NowPlayingPanel::Podcast => StageLayoutKind::Split,
    }
}

/// Renders the full-window / full-screen Now Playing overlay stage.
///
/// Adapts across:
/// - **Desktop**: Minimalist top chevron-down dismiss handle, left-aligned floating
///   tab row (`Lyrics` · `Stage` · `Up Next` · `Wall` · `Comments`), full-window
///   `Stage` & `Wall` views, or spacious borderless 2-column split (`max-w: 1280`,
///   `gap: 72`) with centerpiece, clickable artists, `···` menu, gold `Lossless`
///   badge, borderless white transport, and slim `180pt` volume bar.
/// - **Pad**: Landscape 2-column split / Portrait stacked layout (plus full-window
///   `Stage` & `Wall`) with `>= 44pt` touch targets.
/// - **Phone**: Full-screen sheet with drag pill, chevron-down dismiss handle,
///   segmented switcher (`Cover` / `Lyrics` / `Stage` / `More`), and `>= 44pt`
///   thumb-zone transport controls.
pub fn now_playing_overlay(store: AppStore, form: DeviceForm) -> View {
    // Auto-resolve invalid panel selections whenever source capabilities or episode state change
    effect(move || {
        let cur = store.now_playing_panel.get();
        let eff = panels::effective_panel(store);
        if cur != eff {
            store.set_now_playing_panel(eff);
        }
    });

    view()
        .test_id("now-playing-overlay")
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .overflow_hidden()
        .bg(theme::np_bg)
        .text_color(theme::np_fg)
        // Ambient GPU WGSL fog shader (30fps gated) or clean HSV still backdrop
        .child(backdrop::ambient_backdrop(store))
        // Form-factor-adapted interactive stage foreground
        .child(match form {
            DeviceForm::Desktop => desktop_stage(store, form),
            DeviceForm::Pad { landscape } => pad_stage(store, form, landscape),
            DeviceForm::Phone => phone_sheet_stage(store, form),
        })
}

fn desktop_side_pad_x() -> f32 {
    let w = window().width.0;
    ((w - NP_MAX_WIDTH) * 0.5).max(72.0)
}

fn desktop_cover_size() -> f32 {
    let win = window();
    let side = (win.width.0 * 0.36).min(440.0);
    side.min(win.height.0 - 436.0).clamp(260.0, 420.0)
}

// -----------------------------------------------------------------------------
// Desktop Layout (`ui_nowplaying.go:1268-1404`)
// -----------------------------------------------------------------------------

fn stage_viewport(
    store: AppStore,
    form: DeviceForm,
    layout_kind: Memo<StageLayoutKind>,
    key_prefix: &'static str,
    build_split: impl Fn() -> View + 'static,
) -> View {
    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .child(dynamic(move || {
            let kind = layout_kind.get();
            untracked(|| match kind {
                StageLayoutKind::Stage => {
                    stage::stage_panel(store, form).key(format!("{key_prefix}-stage"))
                }
                StageLayoutKind::Wall => {
                    wall::wall_panel(store, form).key(format!("{key_prefix}-wall"))
                }
                StageLayoutKind::Split => build_split().key(format!("{key_prefix}-split")),
            })
        }))
}

fn desktop_stage(store: AppStore, form: DeviceForm) -> View {
    let pointer_in = signal(true);
    let eff_panel = computed(move || panels::effective_panel(store));
    let layout_kind = computed(move || stage_layout_kind(eff_panel.get()));

    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .on_mouse_enter(move |_: &MouseEvent| pointer_in.set(true))
        .on_mouse_leave(move |_: &MouseEvent| pointer_in.set(false))
        // Centered top chevron-down dismiss handle avoiding macOS traffic lights,
        // Windows/Linux caption buttons, and display notch / safe-area insets
        .child(
            view()
                .w_full()
                .h(move || px(title_bar().top.0.max(safe_area().top.0).max(44.0)))
                .pl(move || px(title_bar().left.0.max(safe_area().left.0) + 12.0))
                .pr(move || px(title_bar().right.0.max(safe_area().right.0) + 12.0))
                .items_center()
                .justify_center()
                .window_drag(true)
                .transition_all()
                .duration(move || motion::transition_ms(200.0))
                .opacity(move || if pointer_in.get() { 1.0_f32 } else { 0.45_f32 })
                .child(
                    view()
                        .window_drag(false)
                        .child(stage_dismiss_button(store, 34.0, 0.70)),
                ),
        )
        // Single left-aligned floating tab row aligned with the cover's left edge
        .child(
            view()
                .w_full()
                .pl(move || px(desktop_side_pad_x().max(safe_area().left.0 + 16.0)))
                .pr(move || px(desktop_side_pad_x().max(safe_area().right.0 + 16.0)))
                .h(px(30.0))
                .flex_row()
                .items_center()
                .gap_1p5()
                .transition_all()
                .duration(move || motion::transition_ms(200.0))
                .opacity(move || if pointer_in.get() { 1.0_f32 } else { 0.55_f32 })
                .child(floating_pane_tabs(store, form, eff_panel))
                .child(when(
                    move || eff_panel.get() == NowPlayingPanel::Stage,
                    move || stage::stage_style_menu_button(store, form),
                )),
        )
        // Main stage viewport: full-window Stage, full-window Wall, or borderless 2-column split
        .child(stage_viewport(
            store,
            form,
            layout_kind,
            "np-desk",
            move || desktop_split_body(store, form),
        ))
}

fn desktop_split_body(store: AppStore, form: DeviceForm) -> View {
    let cover_size = desktop_cover_size();
    let col_w = cover_size.max(280.0);

    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .pl(move || px(desktop_side_pad_x().max(safe_area().left.0 + 16.0)))
        .pr(move || px(desktop_side_pad_x().max(safe_area().right.0 + 16.0)))
        .pt(px(20.0))
        .pb(move || px(safe_area().bottom.0 + 40.0))
        .flex_row()
        .items_stretch()
        .gap(px(72.0))
        // Left column: centerpiece + subtle mode pills + track info + scrubber/lossless + transport + volume
        .child(
            view()
                .w(px(col_w))
                .flex_shrink_0()
                .justify_center()
                .items_start()
                .child(retro::retro_centerpiece(store, cover_size))
                // Unobtrusive Cover / Vinyl / Cassette selector tucked right below centerpiece
                .child(
                    view()
                        .w_full()
                        .mt_3()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .child(compact_mode_pills(store, false)),
                )
                // Title + clickable artist links + Like + lazy `···` menu
                .child(
                    view()
                        .w_full()
                        .mt_5()
                        .mb_4()
                        .child(track_meta_row(store, form, false)),
                )
                // Scrubber + elapsed / Lossless badge / remaining (or Buffering…)
                .child(stage_scrubber_bar(store, false))
                .child(scrubber_meta_row(store))
                // Borderless white transport controls
                .child(
                    view()
                        .w_full()
                        .mt_3p5()
                        .mb_4()
                        .items_center()
                        .child(stage_transport(store, form, 28.0, true)),
                )
                // Slim 180pt volume slider (no `%` noise)
                .child(
                    view()
                        .w_full()
                        .items_center()
                        .child(volume_bar(store, form, 180.0)),
                ),
        )
        // Right pane: COMPLETELY BORDERLESS floating directly over the fog backdrop
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .child(panels::active_panel_content(store, form)),
        )
}

// -----------------------------------------------------------------------------
// Pad Layout: Landscape 2-column split / Portrait stacked (or Stage / Wall) + >=44pt touch
// -----------------------------------------------------------------------------

fn pad_stage(store: AppStore, form: DeviceForm, landscape: bool) -> View {
    let eff_panel = computed(move || panels::effective_panel(store));
    let layout_kind = computed(move || stage_layout_kind(eff_panel.get()));
    let side_pad = if landscape { 24.0 } else { 20.0 };

    let tabs_strip = view()
        .flex_row()
        .items_center()
        .gap(px(if landscape { 6.0 } else { 4.0 }))
        .child(floating_pane_tabs(store, form, eff_panel))
        .child(when(
            move || eff_panel.get() == NowPlayingPanel::Stage,
            move || stage::stage_style_menu_button(store, form),
        ));

    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .pt_safe_area_plus(px(10.0))
        .pb_safe_area_plus(px(14.0))
        .pl(move || px(safe_area().left.0 + side_pad))
        .pr(move || px(safe_area().right.0 + side_pad))
        .gap_3()
        // Minimalist top bar: 44pt dismiss chevron + floating tabs (+ Stage Style menu)
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(if landscape { 12.0 } else { 8.0 }))
                .child(stage_dismiss_button(store, 44.0, 0.75))
                .child(if landscape {
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .child(tabs_strip)
                } else {
                    view().flex_1().min_w(px(0.0)).child(
                        scroll()
                            .horizontal()
                            .scrollbar(false)
                            .w_full()
                            .child(tabs_strip),
                    )
                }),
        )
        .child(stage_viewport(
            store,
            form,
            layout_kind,
            if landscape { "np-padl" } else { "np-padp" },
            move || {
                if landscape {
                    pad_landscape_split_body(store, form)
                } else {
                    pad_portrait_stack_body(store, form)
                }
            },
        ))
}

fn pad_landscape_split_body(store: AppStore, form: DeviceForm) -> View {
    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .flex_row()
        .items_stretch()
        .gap_8()
        .child(
            view()
                .w(px(300.0))
                .flex_shrink_0()
                .justify_center()
                .items_center()
                .gap_3()
                .child(retro::retro_centerpiece(store, 240.0))
                .child(compact_mode_pills(store, true))
                .child(
                    view()
                        .w_full()
                        .gap_2p5()
                        .child(track_meta_row(store, form, false))
                        .child(stage_scrubber_bar(store, false))
                        .child(scrubber_meta_row(store))
                        .child(stage_transport(store, form, 26.0, true))
                        .child(volume_bar(store, form, 180.0)),
                ),
        )
        // Borderless right pane
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .child(panels::active_panel_content(store, form)),
        )
}

fn pad_portrait_stack_body(store: AppStore, form: DeviceForm) -> View {
    view()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .gap_4()
        .child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_5()
                .child(
                    view()
                        .items_center()
                        .gap_2()
                        .child(retro::retro_centerpiece(store, 196.0))
                        .child(compact_mode_pills(store, true)),
                )
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap_2p5()
                        .child(track_meta_row(store, form, false))
                        .child(stage_scrubber_bar(store, false))
                        .child(scrubber_meta_row(store))
                        .child(stage_transport(store, form, 26.0, true)),
                ),
        )
        // Borderless lower panel
        .child(
            view()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .child(panels::active_panel_content(store, form)),
        )
}

// -----------------------------------------------------------------------------
// Phone Layout: Full-screen sheet with drag pill, floating segmented switcher,
// mini track bar on non-Cover tabs, responsive cover, & >=44pt thumb-zone dock
// -----------------------------------------------------------------------------

fn phone_cover_size() -> f32 {
    let win = window();
    (win.width.0 - 56.0)
        .min(win.height.0 * 0.38)
        .clamp(220.0, 320.0)
}

fn phone_sheet_stage(store: AppStore, form: DeviceForm) -> View {
    let section = signal(PhoneSection::Art);
    let is_episode = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(Track::is_episode))
    });
    let more_subpanel = computed(move || match panels::effective_panel(store) {
        NowPlayingPanel::Wall => PhoneMoreSubpanel::Wall,
        NowPlayingPanel::Comments => PhoneMoreSubpanel::Comments,
        NowPlayingPanel::Podcast => PhoneMoreSubpanel::Podcast,
        NowPlayingPanel::Queue | NowPlayingPanel::Lyrics | NowPlayingPanel::Stage => {
            PhoneMoreSubpanel::Queue
        }
    });
    let is_art = computed(move || section.get() == PhoneSection::Art);
    let show_bottom_scrubber = computed(move || {
        let sec = section.get();
        sec != PhoneSection::Stage
            && !(sec == PhoneSection::More && more_subpanel.get() == PhoneMoreSubpanel::Wall)
    });
    let cover_size_memo = computed(move || {
        let raw = phone_cover_size();
        (raw / 4.0).round() * 4.0
    });

    // Clicking the Quiet-Part Top Comment card from Lyrics or Stage jumps into Comments
    effect(move || {
        let p = store.now_playing_panel.get();
        let cur = section.get_untracked();
        if p == NowPlayingPanel::Comments
            && matches!(cur, PhoneSection::Lyrics | PhoneSection::Stage)
        {
            section.set(PhoneSection::More);
        }
    });

    view()
        .relative()
        .flex_1()
        .w_full()
        .h_full()
        .pt_safe_area_plus(px(6.0))
        .pb_safe_area_plus(px(14.0))
        .pl(move || px(safe_area().left.0))
        .pr(move || px(safe_area().right.0))
        .gap_2()
        // Crisp 36×4pt drag pill
        .child(
            view().w_full().items_center().pb_0p5().child(
                view()
                    .w(px(36.0))
                    .h(px(4.0))
                    .rounded_full()
                    .bg(move || theme::np_fg().opacity(0.28)),
            ),
        )
        // Balanced top header: 44pt dismiss chevron + centered floating segmented switcher + 44pt spacer
        .child(
            view()
                .w_full()
                .h(px(44.0))
                .px_3()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_1()
                .child(stage_dismiss_button(store, 44.0, 0.84))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .items_center()
                        .justify_center()
                        .child(phone_section_switcher(store, section)),
                )
                .child(view().w(px(44.0)).h(px(44.0)).flex_shrink_0()),
        )
        // Compact mini track bar on non-Cover Phone tabs so track identity & quick pause stay visible
        .child(when(
            move || section.get() != PhoneSection::Art,
            move || phone_mini_track_bar(store, section),
        ))
        // Primary adaptive stage viewport
        .child(
            view()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .child(dynamic(move || {
                    let cur = section.get();
                    untracked(|| match cur {
                        PhoneSection::Art => view()
                            .key("np-phone-art")
                            .flex_1()
                            .w_full()
                            .px(px(20.0))
                            .items_center()
                            .justify_center()
                            .gap_2p5()
                            .child(dynamic(move || {
                                let sz = cover_size_memo.get();
                                untracked(|| {
                                    retro::retro_centerpiece(store, sz)
                                        .key(format!("np-phone-art-{}", sz as i32))
                                })
                            }))
                            .child(compact_mode_pills(store, true)),

                        PhoneSection::Lyrics => view()
                            .key("np-phone-lyrics")
                            .flex_1()
                            .w_full()
                            .min_h(px(0.0))
                            .child(
                                when(is_episode, move || panels::podcast_panel(store, form))
                                    .otherwise(move || lyrics_view::lyrics_view(store, form)),
                            ),

                        PhoneSection::Stage => view()
                            .key("np-phone-stage")
                            .flex_1()
                            .w_full()
                            .min_h(px(0.0))
                            .gap_1p5()
                            .child(
                                view().w_full().px(px(20.0)).items_center().child(
                                    scroll()
                                        .horizontal()
                                        .scrollbar(false)
                                        .child(stage::stage_style_switcher(store, true)),
                                ),
                            )
                            .child(stage::stage_panel(store, form)),

                        PhoneSection::More => view()
                            .key("np-phone-more")
                            .flex_1()
                            .w_full()
                            .min_h(px(0.0))
                            .gap_1p5()
                            .child(phone_more_subtabs(store, more_subpanel))
                            .child(dynamic(move || {
                                let sub = more_subpanel.get();
                                untracked(|| match sub {
                                    PhoneMoreSubpanel::Wall => {
                                        wall::wall_panel(store, form).key("np-phone-wall")
                                    }
                                    PhoneMoreSubpanel::Comments => {
                                        panels::comments_panel(store, form).key("np-phone-comments")
                                    }
                                    PhoneMoreSubpanel::Podcast => {
                                        panels::podcast_panel(store, form).key("np-phone-podcast")
                                    }
                                    PhoneMoreSubpanel::Queue => {
                                        panels::queue_panel(store, form).key("np-phone-queue")
                                    }
                                })
                            })),
                    })
                })),
        )
        // Single persistent bottom dock & scrubber so the scrubber + 30fps smoke canvas are
        // never torn down/recreated when switching between Cover / Lyrics / More tabs.
        .child(when(show_bottom_scrubber, move || {
            view()
                .w_full()
                .px(px(20.0))
                .pt_1()
                .gap(move || if is_art.get() { px(12.0) } else { px(2.0) })
                .child(when(is_art, move || phone_cover_track_header(store)))
                .child(
                    view()
                        .w_full()
                        .gap_0p5()
                        .child(stage_scrubber_bar(store, false))
                        .child(phone_scrubber_meta_row(store)),
                )
                .child(when(is_art, move || phone_thumb_transport(store, 64.0)))
        }))
}

fn phone_section_switcher(store: AppStore, section: Signal<PhoneSection>) -> View {
    let items: &[(PhoneSection, &str, &str)] = &[
        (
            PhoneSection::Art,
            "now_playing.mode.cover",
            "np-phone-tab-cover",
        ),
        (
            PhoneSection::Lyrics,
            "now_playing.panel.lyrics",
            "np-phone-tab-lyrics",
        ),
        (
            PhoneSection::Stage,
            "now_playing.panel.stage",
            "np-phone-tab-stage",
        ),
        (PhoneSection::More, "common.more", "np-phone-tab-more"),
    ];

    view()
        .relative()
        .h(px(44.0))
        .flex_row()
        .items_center()
        // Sleek 32pt frosted floating pill track centered vertically behind 44pt touch hit-targets
        .child(
            view()
                .absolute()
                .left_0()
                .right_0()
                .top(px(6.0))
                .h(px(32.0))
                .rounded_full()
                .bg(move || theme::np_fg().opacity(0.09))
                .border_1()
                .border_color(move || theme::np_fg().opacity(0.08))
                .pointer_events(PointerEvents::None),
        )
        .children(items.iter().copied().map(move |(sec, key, tid)| {
            let active = move || section.get() == sec;
            button()
                .test_id(tid)
                .selected(active)
                .accessibility_label(move || Some(store.t(key)))
                .on_press(move || {
                    batch(|| {
                        match sec {
                            PhoneSection::Lyrics => {
                                store.set_now_playing_panel(NowPlayingPanel::Lyrics);
                            }
                            PhoneSection::Stage => {
                                store.set_now_playing_panel(NowPlayingPanel::Stage);
                            }
                            PhoneSection::More => {
                                if matches!(
                                    store.now_playing_panel.get_untracked(),
                                    NowPlayingPanel::Lyrics | NowPlayingPanel::Stage
                                ) {
                                    store.set_now_playing_panel(NowPlayingPanel::Queue);
                                }
                            }
                            PhoneSection::Art => {}
                        }
                        section.set(sec);
                    });
                })
                .min_w(px(44.0))
                .min_h(px(44.0))
                .px(px(2.5))
                .items_center()
                .justify_center()
                .bg(colors::TRANSPARENT)
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    view()
                        .h(px(26.0))
                        .px(px(8.5))
                        .rounded_full()
                        .items_center()
                        .justify_center()
                        .bg(move || {
                            if active() {
                                theme::np_fg().opacity(0.20)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .transition_colors()
                        .duration(move || motion::transition_ms(140.0))
                        .child(
                            text(move || store.t(key))
                                .number_of_lines(1)
                                .text_size(px(12.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || {
                                    if active() {
                                        theme::np_fg()
                                    } else {
                                        theme::np_fg().opacity(0.62)
                                    }
                                }),
                        ),
                )
        }))
}

/// Compact mini track header rendered at the top of non-Cover Phone tabs so track identity,
/// quick Play/Pause, and `···` actions remain visible and 1 tap away.
fn phone_mini_track_bar(store: AppStore, section: Signal<PhoneSection>) -> View {
    let has_art = computed(move || store.current_artwork_uri.with(|u| !u.is_empty()));

    view().w_full().px(px(20.0)).child(
        view()
            .w_full()
            .min_h(px(56.0))
            .pl_2p5()
            .pr_1p5()
            .py_1p5()
            .rounded(px(14.0))
            .bg(move || theme::np_fg().opacity(0.08))
            .border_1()
            .border_color(move || theme::np_fg().opacity(0.08))
            .flex_row()
            .items_center()
            .gap_1p5()
            // Tapping cover/title returns smoothly to Cover view
            .child(
                button()
                    .accessibility_label(move || {
                        Some(store.current_track.with(|t| {
                            t.as_ref()
                                .map(|tr| SharedString::from(tr.name.clone()))
                                .unwrap_or_else(|| store.t("player.now_playing"))
                        }))
                    })
                    .on_press(move || section.set(PhoneSection::Art))
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(44.0))
                    .flex_row()
                    .items_center()
                    .gap_2p5()
                    .bg(colors::TRANSPARENT)
                    .active(|s| s.opacity(0.75))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(
                        view()
                            .size(px(40.0))
                            .flex_shrink_0()
                            .rounded(px(8.0))
                            .overflow_hidden()
                            .bg(theme::np_surface)
                            .border_1()
                            .border_color(move || theme::np_fg().opacity(0.10))
                            .items_center()
                            .justify_center()
                            .child(when(
                                move || !has_art.get(),
                                || icon(Icon::Disc, 16.0, colors::NP_MUTED).opacity(0.55),
                            ))
                            .child(when(has_art, move || {
                                image(move || store.current_artwork_uri.get())
                                    .object_fit(ResizeMode::Cover)
                                    .absolute()
                                    .inset_0()
                                    .size_full()
                            })),
                    )
                    .child(
                        view()
                            .flex_1()
                            .min_w(px(0.0))
                            .gap_0p5()
                            .child(
                                text(move || {
                                    store.current_track.with(|t| {
                                        t.as_ref()
                                            .map(|tr| SharedString::from(tr.name.clone()))
                                            .unwrap_or_else(|| store.t("player.idle_title"))
                                    })
                                })
                                .number_of_lines(1)
                                .text_size(px(14.5))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::np_fg),
                            )
                            .child(
                                text(move || {
                                    let fallback = store.t("track.unknown_artist");
                                    store.current_track.with(|t| {
                                        t.as_ref()
                                            .map(|tr| {
                                                let names = tr.artist_names();
                                                if names.is_empty() {
                                                    fallback.clone()
                                                } else {
                                                    SharedString::from(names)
                                                }
                                            })
                                            .unwrap_or(fallback)
                                    })
                                })
                                .number_of_lines(1)
                                .text_size(px(12.5))
                                .text_color(move || theme::np_fg().opacity(0.64)),
                            ),
                    ),
            )
            // Quick Play/Pause button (>= 44pt hit-target with sleek 34pt inner frosted circle)
            .child(
                button()
                    .accessibility_label(move || {
                        Some(if store.is_playing.get() {
                            store.t("player.pause")
                        } else {
                            store.t("player.play")
                        })
                    })
                    .on_press(move || store.toggle_play())
                    .min_w(px(44.0))
                    .min_h(px(44.0))
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(colors::TRANSPARENT)
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(
                        view()
                            .size(px(34.0))
                            .rounded_full()
                            .bg(move || theme::np_fg().opacity(0.14))
                            .items_center()
                            .justify_center()
                            .child(
                                when(
                                    move || store.playback_status.with(PlaybackStatus::is_loading),
                                    move || loading_spinner(16.0, theme::np_fg),
                                )
                                .otherwise(move || {
                                    view()
                                        .pl(move || {
                                            px(if store.is_playing.get() { 0.0 } else { 1.5 })
                                        })
                                        .child(icon_reactive(
                                            move || {
                                                if store.is_playing.get() {
                                                    Icon::Pause
                                                } else {
                                                    Icon::Play
                                                }
                                            },
                                            16.0,
                                            theme::np_fg,
                                        ))
                                }),
                            ),
                    ),
            )
            .child(song_more_menu(store, 19.0, true)),
    )
}

/// Phone Cover view track header: expressive 20pt bold title + 15pt clickable artist + Like & `···`.
fn phone_cover_track_header(store: AppStore) -> View {
    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap_1()
                .child(
                    text(move || {
                        store.current_track.with(|t| {
                            t.as_ref()
                                .map(|tr| SharedString::from(tr.name.clone()))
                                .unwrap_or_else(|| store.t("player.idle_title"))
                        })
                    })
                    .number_of_lines(1)
                    .text_size(px(20.0))
                    .font_weight(FontWeight::BOLD)
                    .letter_spacing(px(-0.35))
                    .text_color(theme::np_fg),
                )
                .child(clickable_artist_links(store, 15.0, 0.68)),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap_0p5()
                .child(stage_like_button(store, 22.0, true))
                .child(song_more_menu(store, 21.0, true)),
        )
}

/// Phone scrubber meta row: elapsed · centered gold `Lossless` OR crisp quality badge · remaining/Buffering.
pub(crate) fn phone_scrubber_meta_row(store: AppStore) -> View {
    let has_track = computed(move || store.current_track.with(Option::is_some));

    view()
        .w_full()
        .h(px(20.0))
        .flex_row()
        .items_center()
        .justify_between()
        // Left: elapsed time (follows live scrubber drag preview when scrubbing)
        .child(
            text(move || {
                let pos = store
                    .scrubber_preview_ms
                    .get()
                    .unwrap_or_else(|| store.position_ms.get());
                format_duration_ms(pos as i64)
            })
            .tabular_nums()
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(move || theme::np_fg().opacity(0.56)),
        )
        // Center: gold `Lossless` badge when stream is lossless, else subtle quality badge (`High`/`Standard`)
        .child(when(has_track, move || {
            when(
                move || store.current_is_lossless.get(),
                move || {
                    view()
                        .px_2()
                        .py_0p5()
                        .rounded(px(5.0))
                        .bg(move || theme::lossless_gold().opacity(0.10))
                        .border_1()
                        .border_color(move || theme::lossless_gold().opacity(0.55))
                        .child(
                            text(move || store.t("player.lossless"))
                                .text_size(px(9.5))
                                .font_weight(FontWeight::EXTRA_BOLD)
                                .letter_spacing(px(0.5))
                                .text_color(theme::lossless_gold),
                        )
                },
            )
            .otherwise(move || {
                view()
                    .px_2()
                    .py_0p5()
                    .rounded(px(5.0))
                    .bg(move || theme::np_fg().opacity(0.08))
                    .border_1()
                    .border_color(move || theme::np_fg().opacity(0.14))
                    .child(
                        text(move || {
                            let key = store.settings.with(|s| match s.audio_quality {
                                Quality::Lossless | Quality::High => "player.quality.high",
                                Quality::Standard => "player.quality.standard",
                            });
                            store.t(key)
                        })
                        .text_size(px(9.5))
                        .font_weight(FontWeight::BOLD)
                        .letter_spacing(px(0.4))
                        .text_color(move || theme::np_fg().opacity(0.68)),
                    )
            })
        }))
        // Right: buffering spinner or `-MM:SS` remaining
        .child(
            when(
                move || store.playback_status.with(PlaybackStatus::is_loading),
                move || buffering_badge(store),
            )
            .otherwise(move || {
                text(move || {
                    let dur = store.duration_ms.get();
                    let pos = store
                        .scrubber_preview_ms
                        .get()
                        .unwrap_or_else(|| store.position_ms.get())
                        .min(dur);
                    let rem = dur.saturating_sub(pos);
                    format!("-{}", format_duration_ms(rem as i64))
                })
                .tabular_nums()
                .text_size(px(11.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || theme::np_fg().opacity(0.56))
            }),
        )
}

/// Generously spaced Phone thumb-zone transport (`Shuffle`, `Prev`, prominent Play/Pause circle,
/// `Next`, `Repeat` — all `>= 44pt` touch targets, evenly distributed across 360..430pt width).
pub(crate) fn phone_thumb_transport(store: AppStore, play_circle_pt: f32) -> View {
    let play_glyph = (play_circle_pt * 0.44).clamp(22.0, 28.0);
    let skip_glyph = if play_circle_pt >= 60.0 { 28.0 } else { 24.0 };
    let aux_glyph = if play_circle_pt >= 60.0 { 20.0 } else { 18.0 };

    let active_tint =
        move || AdaptiveColor::from(theme::artwork_np_accent(store.current_accent.get()));

    view().w_full().items_center().pt_0p5().child(
        view()
            .w_full()
            .max_w(px(360.0))
            .flex_row()
            .items_center()
            .justify_between()
            // Shuffle (>= 44pt) with active indicator dot
            .child(phone_toggle_transport_btn(
                Icon::Shuffle,
                aux_glyph,
                move || store.shuffle.get(),
                active_tint,
                move || {
                    if store.shuffle.get() {
                        store.t("player.shuffle_on")
                    } else {
                        store.t("player.shuffle_off")
                    }
                },
                move || store.toggle_shuffle(),
            ))
            // Previous (48pt)
            .child(stage_icon_button(
                Icon::Prev,
                48.0,
                skip_glyph,
                theme::np_fg,
                move || store.t("player.prev"),
                move || store.prev_track(),
            ))
            // Prominent Play/Pause circle (64pt on Cover, 54pt on Stage/Wall)
            .child(Elevated::elevation(
                button()
                    .accessibility_label(move || {
                        Some(if store.is_playing.get() {
                            store.t("player.pause")
                        } else {
                            store.t("player.play")
                        })
                    })
                    .on_press(move || store.toggle_play())
                    .size(px(play_circle_pt))
                    .min_w(px(play_circle_pt))
                    .min_h(px(play_circle_pt))
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(theme::np_fg)
                    .transition_all()
                    .duration(move || motion::transition_ms(120.0))
                    .active(|s| s.scale(motion::press_scale))
                    .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                    .child(
                        when(
                            move || store.playback_status.with(PlaybackStatus::is_loading),
                            move || loading_spinner(play_glyph * 0.92, theme::np_bg),
                        )
                        .otherwise(move || {
                            view()
                                .pl(move || px(if store.is_playing.get() { 0.0 } else { 2.5 }))
                                .child(icon_reactive(
                                    move || {
                                        if store.is_playing.get() {
                                            Icon::Pause
                                        } else {
                                            Icon::Play
                                        }
                                    },
                                    play_glyph,
                                    theme::np_bg,
                                ))
                        }),
                    ),
                Elevation::Floating,
            ))
            // Next (48pt)
            .child(stage_icon_button(
                Icon::Next,
                48.0,
                skip_glyph,
                theme::np_fg,
                move || store.t("player.next"),
                move || store.next_track(),
            ))
            // Repeat (>= 44pt) with active indicator dot
            .child(phone_toggle_transport_btn(
                move || match store.repeat.get() {
                    RepeatMode::One => Icon::RepeatOne,
                    RepeatMode::Off | RepeatMode::All => Icon::Repeat,
                },
                aux_glyph,
                move || store.repeat.get() != RepeatMode::Off,
                active_tint,
                move || match store.repeat.get() {
                    RepeatMode::Off => store.t("player.repeat_off"),
                    RepeatMode::All => store.t("player.repeat_all"),
                    RepeatMode::One => store.t("player.repeat_one"),
                },
                move || store.cycle_repeat(),
            )),
    )
}

fn phone_toggle_transport_btn(
    glyph: impl IntoReactive<Icon>,
    glyph_size: f32,
    active: impl IntoReactive<bool>,
    active_tint: impl IntoReactive<AdaptiveColor>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let active_rx = active.into_reactive();
    let active_tint_rx = active_tint.into_reactive();
    let label_rx = label.into_reactive();
    let active_for_color = active_rx.clone();
    let active_for_dot = active_rx.clone();
    let tint_for_dot = active_tint_rx.clone();

    button()
        .selected(move || active_rx.get())
        .accessibility_label(move || Some(label_rx.get()))
        .on_press(on_press)
        .min_w(px(44.0))
        .min_h(px(44.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap_0p5()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(120.0))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, glyph_size, move || {
            if active_for_color.get() {
                active_tint_rx.get()
            } else {
                theme::np_fg().opacity(0.54)
            }
        }))
        .child(view().size(px(4.0)).rounded_full().bg(move || {
            if active_for_dot.get() {
                tint_for_dot.get()
            } else {
                colors::TRANSPARENT
            }
        }))
}

fn comments_tab_label(store: AppStore) -> SharedString {
    let base = store.t("now_playing.panel.comments");
    let count = store
        .comments_total
        .get()
        .max(store.comments.with(Vec::len));
    if count > 0 {
        SharedString::from(format!("{base} {}", format_compact_count(count)))
    } else {
        base
    }
}

fn phone_more_subtabs(store: AppStore, more_subpanel: Memo<PhoneMoreSubpanel>) -> View {
    let is_episode = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(Track::is_episode))
    });

    view()
        .w_full()
        .px(px(20.0))
        .flex_row()
        .items_center()
        .justify_center()
        .gap_1()
        .child(phone_subtab_pill(
            store,
            more_subpanel,
            PhoneMoreSubpanel::Queue,
            NowPlayingPanel::Queue,
            move || store.t("now_playing.panel.queue"),
            "now_playing.panel.queue",
            "np-more-tab-queue",
        ))
        .child(when(
            move || !is_episode.get(),
            move || {
                phone_subtab_pill(
                    store,
                    more_subpanel,
                    PhoneMoreSubpanel::Wall,
                    NowPlayingPanel::Wall,
                    move || store.t("now_playing.panel.wall"),
                    "now_playing.panel.wall",
                    "np-more-tab-wall",
                )
            },
        ))
        .child(when(
            move || store.active_can("comments"),
            move || {
                phone_subtab_pill(
                    store,
                    more_subpanel,
                    PhoneMoreSubpanel::Comments,
                    NowPlayingPanel::Comments,
                    move || comments_tab_label(store),
                    "now_playing.panel.comments",
                    "np-more-tab-comments",
                )
            },
        ))
        .child(when(is_episode, move || {
            phone_subtab_pill(
                store,
                more_subpanel,
                PhoneMoreSubpanel::Podcast,
                NowPlayingPanel::Podcast,
                move || store.t("now_playing.panel.podcast"),
                "now_playing.panel.podcast",
                "np-more-tab-podcast",
            )
        }))
}

fn phone_subtab_pill(
    store: AppStore,
    more_subpanel: Memo<PhoneMoreSubpanel>,
    subpanel: PhoneMoreSubpanel,
    panel: NowPlayingPanel,
    label_text: impl Fn() -> SharedString + 'static,
    a11y_key: &'static str,
    tid: &'static str,
) -> impl IntoElement {
    let active = move || more_subpanel.get() == subpanel;
    button()
        .test_id(tid)
        .selected(active)
        .accessibility_label(move || Some(store.t(a11y_key)))
        .on_press(move || batch(|| store.set_now_playing_panel(panel)))
        .min_w(px(44.0))
        .min_h(px(44.0))
        .px(px(2.0))
        .items_center()
        .justify_center()
        .bg(colors::TRANSPARENT)
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .h(px(30.0))
                .px_3p5()
                .rounded_full()
                .items_center()
                .justify_center()
                .bg(move || {
                    if active() {
                        theme::np_fg().opacity(0.16)
                    } else {
                        theme::np_fg().opacity(0.06)
                    }
                })
                .border_1()
                .border_color(move || {
                    if active() {
                        theme::np_fg().opacity(0.14)
                    } else {
                        colors::TRANSPARENT
                    }
                })
                .transition_colors()
                .duration(move || motion::transition_ms(140.0))
                .child(
                    text(label_text)
                        .tabular_nums()
                        .number_of_lines(1)
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(move || {
                            if active() {
                                theme::np_fg()
                            } else {
                                theme::np_fg().opacity(0.62)
                            }
                        }),
                ),
        )
}

// -----------------------------------------------------------------------------
// Floating Minimalist Pane Tabs (`ui_nowplaying.go:1443-1566`)
// -----------------------------------------------------------------------------

fn floating_pane_tabs(store: AppStore, form: DeviceForm, eff_panel: Memo<NowPlayingPanel>) -> View {
    let touch = form.is_touch();
    let is_episode = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(Track::is_episode))
    });

    view()
        .flex_row()
        .items_center()
        .gap_1p5()
        // Song tabs: Lyrics · Stage · Up Next · Wall
        .child(when(
            move || !is_episode.get(),
            move || {
                view()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .child(floating_tab_btn(
                        store,
                        eff_panel,
                        NowPlayingPanel::Lyrics,
                        move || store.t("now_playing.panel.lyrics"),
                        "now_playing.panel.lyrics",
                        "np-tab-lyrics",
                        touch,
                    ))
                    .child(floating_tab_btn(
                        store,
                        eff_panel,
                        NowPlayingPanel::Stage,
                        move || store.t("now_playing.panel.stage"),
                        "now_playing.panel.stage",
                        "np-tab-stage",
                        touch,
                    ))
                    .child(floating_tab_btn(
                        store,
                        eff_panel,
                        NowPlayingPanel::Queue,
                        move || store.t("now_playing.panel.queue"),
                        "now_playing.panel.queue",
                        "np-tab-queue",
                        touch,
                    ))
                    .child(floating_tab_btn(
                        store,
                        eff_panel,
                        NowPlayingPanel::Wall,
                        move || store.t("now_playing.panel.wall"),
                        "now_playing.panel.wall",
                        "np-tab-wall",
                        touch,
                    ))
            },
        ))
        // Episode tabs: Podcast (About) · Up Next
        .child(when(is_episode, move || {
            view()
                .flex_row()
                .items_center()
                .gap_1p5()
                .child(floating_tab_btn(
                    store,
                    eff_panel,
                    NowPlayingPanel::Podcast,
                    move || store.t("now_playing.panel.podcast"),
                    "now_playing.panel.podcast",
                    "np-tab-podcast",
                    touch,
                ))
                .child(floating_tab_btn(
                    store,
                    eff_panel,
                    NowPlayingPanel::Queue,
                    move || store.t("now_playing.panel.queue"),
                    "now_playing.panel.queue",
                    "np-tab-queue",
                    touch,
                ))
        }))
        // Capability-gated Comments tab with live count (`ui_nowplaying.go:1555-1562`)
        .child(when(
            move || store.active_can("comments"),
            move || {
                floating_tab_btn(
                    store,
                    eff_panel,
                    NowPlayingPanel::Comments,
                    move || comments_tab_label(store),
                    "now_playing.panel.comments",
                    "np-tab-comments",
                    touch,
                )
            },
        ))
}

fn floating_tab_btn(
    store: AppStore,
    eff_panel: Memo<NowPlayingPanel>,
    panel: NowPlayingPanel,
    label_text: impl Fn() -> SharedString + 'static,
    a11y_key: &'static str,
    tid: &'static str,
    touch: bool,
) -> impl IntoElement {
    let active = move || eff_panel.get() == panel;

    button()
        .test_id(tid)
        .selected(active)
        .accessibility_label(move || Some(store.t(a11y_key)))
        .on_press(move || batch(|| store.set_now_playing_panel(panel)))
        .min_w(px(if touch { 44.0 } else { 0.0 }))
        .min_h(px(if touch { 44.0 } else { 28.0 }))
        .px_3p5()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(move || {
            if active() {
                theme::np_fg().opacity(0.14)
            } else {
                colors::TRANSPARENT
            }
        })
        .transition_colors()
        .duration(move || motion::transition_ms(120.0))
        .hover(|s| s.bg(move || theme::np_fg().opacity(0.08)))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            text(label_text)
                .tabular_nums()
                .number_of_lines(1)
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(move || {
                    if active() {
                        theme::np_fg()
                    } else {
                        theme::np_fg().opacity(0.55)
                    }
                }),
        )
}

fn format_compact_count(n: usize) -> String {
    if n >= 10_000 {
        format!("{:.1}w", n as f64 / 10_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// -----------------------------------------------------------------------------
// Compact Cover / Vinyl / Cassette Pills (under centerpiece, never crowding top bar)
// -----------------------------------------------------------------------------

fn compact_mode_pills(store: AppStore, touch: bool) -> View {
    let modes: &[(NowPlayingMode, &str, &str)] = &[
        (
            NowPlayingMode::Cover,
            "now_playing.mode.cover",
            "np-mode-cover",
        ),
        (
            NowPlayingMode::Vinyl,
            "now_playing.mode.vinyl",
            "np-mode-vinyl",
        ),
        (
            NowPlayingMode::Cassette,
            "now_playing.mode.cassette",
            "np-mode-cassette",
        ),
    ];

    view()
        .flex_row()
        .items_center()
        .gap_1()
        .children(modes.iter().copied().map(move |(mode, key, tid)| {
            let active = move || store.now_playing_mode.get() == mode;
            button()
                .test_id(tid)
                .selected(active)
                .accessibility_label(move || Some(store.t(key)))
                .on_press(move || batch(|| store.set_now_playing_mode(mode)))
                .min_h(px(if touch { 44.0 } else { 24.0 }))
                .min_w(px(if touch { 44.0 } else { 0.0 }))
                .px(px(if touch { 2.0 } else { 0.0 }))
                .items_center()
                .justify_center()
                .bg(colors::TRANSPARENT)
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    view()
                        .h(px(if touch { 28.0 } else { 24.0 }))
                        .px(px(if touch { 12.0 } else { 10.0 }))
                        .rounded_full()
                        .items_center()
                        .justify_center()
                        .bg(move || {
                            if active() {
                                theme::np_fg().opacity(if touch { 0.16 } else { 0.12 })
                            } else if touch {
                                theme::np_fg().opacity(0.06)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .border_1()
                        .border_color(move || {
                            if touch && active() {
                                theme::np_fg().opacity(0.14)
                            } else {
                                colors::TRANSPARENT
                            }
                        })
                        .transition_colors()
                        .duration(move || motion::transition_ms(120.0))
                        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
                        .child(
                            text(move || store.t(key))
                                .number_of_lines(1)
                                .text_size(px(if touch { 12.0 } else { 11.5 }))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(move || {
                                    if active() {
                                        theme::np_fg().opacity(0.95)
                                    } else {
                                        theme::np_fg().opacity(if touch { 0.56 } else { 0.45 })
                                    }
                                }),
                        ),
                )
        }))
}

// -----------------------------------------------------------------------------
// Shared Stage Controls (Clickable Artists, Song Menu, Scrubber/Lossless, Transport, Volume)
// -----------------------------------------------------------------------------

/// Clickable artist links navigating to `Route::ArtistDetail` (closing Now Playing first),
/// matching `mygo-music` `artistLinks`.
pub(crate) fn clickable_artist_links(store: AppStore, size_pt: f32, muted_alpha: f32) -> View {
    view().w_full().min_w(px(0.0)).child(dynamic(move || {
        let artists = store
            .current_track
            .with(|t| t.as_ref().map(|tr| tr.artists.clone()).unwrap_or_default());
        let can_artist = store.active_can("artist");
        let fallback = store.t("track.unknown_artist");

        untracked(|| {
            let valid: Vec<_> = artists
                .into_iter()
                .filter(|a| !a.name.trim().is_empty())
                .collect();
            if valid.is_empty() {
                return view().child(
                    text(fallback)
                        .number_of_lines(1)
                        .text_size(px(size_pt))
                        .text_color(move || theme::np_fg().opacity(muted_alpha)),
                );
            }

            let len = valid.len();
            let mut row = view().w_full().flex_row().items_center().flex_wrap();
            for (i, ar) in valid.into_iter().enumerate() {
                let is_clickable = can_artist && !ar.id.is_empty();
                let name = SharedString::from(ar.name.clone());
                if is_clickable {
                    let aid = ar.id.clone();
                    let label_a11y = name.clone();
                    row = row.child(
                        button()
                            .accessibility_label(Some(label_a11y))
                            .hit_slop(EdgeInsets::all(px(14.0)))
                            .on_press(move || {
                                batch(|| {
                                    store.close_now_playing();
                                    store.navigate(Route::ArtistDetail { id: aid.clone() });
                                });
                            })
                            .bg(colors::TRANSPARENT)
                            .hover(|s| s.text_color(theme::np_fg))
                            .active(|s| s.opacity(0.75))
                            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                            .child(
                                text(name)
                                    .number_of_lines(1)
                                    .text_size(px(size_pt))
                                    .text_color(move || theme::np_fg().opacity(muted_alpha)),
                            ),
                    );
                } else {
                    row = row.child(
                        text(name)
                            .number_of_lines(1)
                            .text_size(px(size_pt))
                            .text_color(move || theme::np_fg().opacity(muted_alpha)),
                    );
                }
                if i + 1 < len {
                    row = row.child(
                        text(", ")
                            .text_size(px(size_pt))
                            .text_color(move || theme::np_fg().opacity(muted_alpha)),
                    );
                }
            }
            row
        })
    }))
}

fn build_song_more_actions(store: AppStore) -> Vec<ContextMenuAction> {
    let Some(tr) = store.current_track.get_untracked() else {
        return Vec::new();
    };

    let mut actions = vec![
        ContextMenuAction::new(store.t("player.play_next"), {
            let t = tr.clone();
            move || store.enqueue_next(t.clone())
        })
        .icon(Icon::Queue),
        ContextMenuAction::new(store.t("player.add_to_queue"), {
            let t = tr.clone();
            move || store.enqueue(t.clone())
        })
        .icon(Icon::Plus),
    ];

    // Navigation links (close Now Playing cleanly before navigating)
    let mut first_nav = true;
    if !tr.show_id.is_empty() && store.active_can("show") {
        let sid = tr.show_id.clone();
        let mut act = ContextMenuAction::new(store.t("track.go_to_podcast"), move || {
            batch(|| {
                store.close_now_playing();
                store.navigate(Route::ShowDetail { id: sid.clone() });
            });
        })
        .icon(Icon::Podcast);
        if first_nav {
            act = act.separator_before();
            first_nav = false;
        }
        actions.push(act);
    }
    if let Some(ar) = tr.artists.first()
        && !ar.id.is_empty()
        && store.active_can("artist")
    {
        let aid = ar.id.clone();
        let mut act = ContextMenuAction::new(store.t("now_playing.go_to_artist"), move || {
            batch(|| {
                store.close_now_playing();
                store.navigate(Route::ArtistDetail { id: aid.clone() });
            });
        })
        .icon(Icon::User);
        if first_nav {
            act = act.separator_before();
            first_nav = false;
        }
        actions.push(act);
    }
    if !tr.album.id.is_empty() && store.active_can("album") {
        let aid = tr.album.id.clone();
        let mut act = ContextMenuAction::new(store.t("now_playing.go_to_album"), move || {
            batch(|| {
                store.close_now_playing();
                store.navigate(Route::AlbumDetail { id: aid.clone() });
            });
        })
        .icon(Icon::Disc);
        if first_nav {
            act = act.separator_before();
        }
        actions.push(act);
    }

    // Copy & Download actions
    actions.push(
        ContextMenuAction::new(store.t("track.copy_title"), {
            let t = tr.clone();
            move || store.copy_track_title(&t)
        })
        .separator_before(),
    );
    actions.push(ContextMenuAction::new(
        store.t("track.copy_title_artists"),
        {
            let t = tr.clone();
            move || store.copy_track_title_and_artists(&t)
        },
    ));
    if !tr.url.is_empty() {
        actions.push(
            ContextMenuAction::new(store.t("track.copy_link"), {
                let t = tr.clone();
                move || store.copy_track_link(&t)
            })
            .icon(Icon::External),
        );
    }
    if store.can_download_track(&tr) {
        actions.push(
            ContextMenuAction::new(store.t("track.download"), {
                let t = tr.clone();
                move || store.download_track(t.clone())
            })
            .icon(Icon::Download),
        );
    }

    actions
}

/// `···` song action menu (`Play Next`, `Add to Queue`, `Go to Artist`, `Go to Album`,
/// `Copy Title`, `Download`) matching `mygo-music` `songMenu`.
/// Actions are built lazily only when opened via [`styled_lazy_menu`], with a flat
/// non-nested trigger view.
pub(crate) fn song_more_menu(store: AppStore, glyph_size: f32, touch: bool) -> View {
    let target = if touch { 44.0 } else { 32.0 };
    let has_track = computed(move || store.current_track.with(Option::is_some));

    view().flex_shrink_0().child(when(has_track, move || {
        let trigger = view()
            .test_id("np-song-more-btn")
            .role(AccessibilityRole::Button)
            .label(move || store.t("common.more"))
            .min_w(px(target))
            .min_h(px(target))
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(colors::TRANSPARENT)
            .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
            .active(|s| s.scale(motion::press_scale))
            .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
            .child(icon(Icon::MoreHorizontal, glyph_size, colors::NP_FG).opacity(0.70));

        styled_lazy_menu(trigger, move || build_song_more_actions(store))
    }))
}

fn track_meta_row(store: AppStore, form: DeviceForm, compact: bool) -> View {
    let touch = form.is_touch();
    let action_glyph = if form.is_desktop() { 22.0 } else { 20.0 };
    view()
        .w_full()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(if form.is_desktop() { 4.0 } else { 2.0 }))
                .child(
                    text(move || {
                        store.current_track.with(|t| {
                            t.as_ref()
                                .map(|tr| SharedString::from(tr.name.clone()))
                                .unwrap_or_else(|| store.t("player.idle_title"))
                        })
                    })
                    .number_of_lines(1)
                    .text_size(px(if compact { 19.0 } else { 22.0 }))
                    .font_weight(FontWeight::BOLD)
                    .letter_spacing(px(-0.3))
                    .text_color(theme::np_fg),
                )
                .child(clickable_artist_links(
                    store,
                    if compact { 14.0 } else { 15.0 },
                    0.60,
                )),
        )
        .child(stage_like_button(store, action_glyph, touch))
        .children((!compact).then(|| song_more_menu(store, action_glyph, touch)))
}

/// Smoke-enabled scrubber wired to `store.settings.smoke_progress`, `is_playing`, and `current_accent`.
pub(crate) fn stage_scrubber_bar(store: AppStore, show_labels: bool) -> impl IntoElement {
    let buffered_ratio = move || {
        let dur = store.duration_ms.get();
        if dur == 0 {
            0.0_f32
        } else {
            (store.buffered_ms.get() as f32 / dur as f32).clamp(0.0, 1.0)
        }
    };

    scrubber(store.position_ms, store.duration_ms)
        .buffered_ratio(buffered_ratio)
        .smoke_mode(move || store.settings.with(|s| s.smoke_progress))
        .playing(move || store.is_playing.get())
        .accent(move || store.current_accent.get())
        .show_labels(show_labels)
        .on_dark_stage(true)
        .on_preview(move |ms| store.scrubber_preview_ms.set(ms))
        .on_seek(move |ms| store.seek(ms))
}

/// Dedicated `16pt` row under the scrubber: elapsed · gold `Lossless` badge · remaining/Buffering
/// (`ui_nowplaying.go:1331-1341`, constant height so column never jumps when lossless toggles).
fn scrubber_meta_row(store: AppStore) -> View {
    view()
        .w_full()
        .h(px(18.0))
        .mt_0p5()
        .flex_row()
        .items_center()
        .justify_between()
        // Left: elapsed time (follows live scrubber drag preview when scrubbing)
        .child(
            text(move || {
                let pos = store
                    .scrubber_preview_ms
                    .get()
                    .unwrap_or_else(|| store.position_ms.get());
                format_duration_ms(pos as i64)
            })
            .tabular_nums()
            .text_size(px(11.0))
            .text_color(move || theme::np_fg().opacity(0.50)),
        )
        // Center: gold `Lossless` badge (`ui_nowplaying.go:1501-1507`)
        .child(when(
            move || store.current_is_lossless.get(),
            move || {
                view()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(move || theme::lossless_gold().opacity(0.55))
                    .child(
                        text(move || store.t("player.lossless"))
                            .text_size(px(9.5))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .letter_spacing(px(0.6))
                            .text_color(theme::lossless_gold),
                    )
            },
        ))
        // Right: animated `loading_spinner` + `Buffering…` when loading, else `-MM:SS` remaining
        .child(
            when(
                move || store.playback_status.with(PlaybackStatus::is_loading),
                move || buffering_badge(store),
            )
            .otherwise(move || {
                text(move || {
                    let dur = store.duration_ms.get();
                    let pos = store
                        .scrubber_preview_ms
                        .get()
                        .unwrap_or_else(|| store.position_ms.get())
                        .min(dur);
                    let rem = dur.saturating_sub(pos);
                    format!("-{}", format_duration_ms(rem as i64))
                })
                .tabular_nums()
                .text_size(px(11.0))
                .text_color(move || theme::np_fg().opacity(0.50))
            }),
        )
}

/// Animated spinner + localized `player.buffering` indicator (`#63`).
pub(crate) fn buffering_badge(store: AppStore) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_1()
        .child(loading_spinner(11.0, move || theme::np_fg().opacity(0.65)))
        .child(
            text(move || store.t("player.buffering"))
                .text_size(px(11.0))
                .text_color(move || theme::np_fg().opacity(0.60)),
        )
}

/// Clean borderless white transport (`ui_player.go:2158-2188`):
/// Shuffle (`0.62x`), Prev (`0.85x`), pure white Play/Pause (`1.25x` — NO red circle!),
/// Next (`0.85x`), Repeat (`0.62x`). Active Shuffle/Repeat tint with `artwork_np_accent`
/// on the main split stage or pure white on `stageBar`/`wallBar`.
pub(crate) fn stage_transport(
    store: AppStore,
    form: DeviceForm,
    size_pt: f32,
    tint_active_with_accent: bool,
) -> View {
    let touch = form.is_touch();
    let aux_box = if touch {
        44.0
    } else {
        (size_pt * 1.15).max(32.0)
    };
    let skip_box = if touch {
        44.0
    } else {
        (size_pt * 1.35).max(36.0)
    };
    let play_box = if touch {
        52.0
    } else {
        (size_pt * 1.60).max(42.0)
    };
    let gap = if touch { 12.0 } else { size_pt * 0.50 };

    let active_tint = move || {
        if tint_active_with_accent {
            AdaptiveColor::from(theme::artwork_np_accent(store.current_accent.get()))
        } else {
            theme::np_fg()
        }
    };

    view()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(gap))
        // Shuffle
        .child(stage_toggle_icon(
            Icon::Shuffle,
            aux_box,
            size_pt * 0.62,
            move || store.shuffle.get(),
            active_tint,
            move || {
                if store.shuffle.get() {
                    store.t("player.shuffle_on")
                } else {
                    store.t("player.shuffle_off")
                }
            },
            move || store.toggle_shuffle(),
        ))
        // Previous
        .child(stage_icon_button(
            Icon::Prev,
            skip_box,
            size_pt * 0.85,
            theme::np_fg,
            move || store.t("player.prev"),
            move || store.prev_track(),
        ))
        // Pure white borderless Play / Pause (shows spinner if buffering)
        .child(
            button()
                .test_id("np-play-pause-btn")
                .accessibility_label(move || {
                    Some(if store.is_playing.get() {
                        store.t("player.pause")
                    } else {
                        store.t("player.play")
                    })
                })
                .on_press(move || store.toggle_play())
                .min_w(px(play_box))
                .min_h(px(play_box))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(colors::TRANSPARENT)
                .transition_all()
                .duration(move || motion::transition_ms(120.0))
                .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
                .active(|s| s.scale(motion::press_scale))
                .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
                .child(
                    when(
                        move || store.playback_status.with(PlaybackStatus::is_loading),
                        move || loading_spinner(size_pt * 1.05, theme::np_fg),
                    )
                    .otherwise(move || {
                        icon_reactive(
                            move || {
                                if store.is_playing.get() {
                                    Icon::Pause
                                } else {
                                    Icon::Play
                                }
                            },
                            size_pt * 1.25,
                            theme::np_fg,
                        )
                    }),
                ),
        )
        // Next
        .child(stage_icon_button(
            Icon::Next,
            skip_box,
            size_pt * 0.85,
            theme::np_fg,
            move || store.t("player.next"),
            move || store.next_track(),
        ))
        // Repeat
        .child(stage_toggle_icon(
            move || match store.repeat.get() {
                RepeatMode::One => Icon::RepeatOne,
                RepeatMode::Off | RepeatMode::All => Icon::Repeat,
            },
            aux_box,
            size_pt * 0.62,
            move || store.repeat.get() != RepeatMode::Off,
            active_tint,
            move || match store.repeat.get() {
                RepeatMode::Off => store.t("player.repeat_off"),
                RepeatMode::All => store.t("player.repeat_all"),
                RepeatMode::One => store.t("player.repeat_one"),
            },
            move || store.cycle_repeat(),
        ))
}

/// Slim centered volume bar matching `mygo-music` `volumeSlider` (`ui_player.go:2135-2155`) —
/// speaker mute toggle + clean `180pt` 4pt->5pt slider, ZERO `%` text noise.
fn volume_bar(store: AppStore, form: DeviceForm, slider_w: f32) -> View {
    let touch = form.is_touch();
    let eff_volume = move || {
        if store.muted.get() {
            0.0_f64
        } else {
            store.volume.get() as f64
        }
    };
    let mute_size = if touch { 44.0 } else { 28.0 };

    view()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_1p5()
        .child(stage_icon_button(
            move || {
                if store.muted.get() || store.volume.get() <= 0.001 {
                    Icon::VolumeMute
                } else {
                    Icon::Volume
                }
            },
            mute_size,
            16.0,
            move || theme::np_fg().opacity(0.70),
            move || {
                if store.muted.get() {
                    store.t("player.unmute")
                } else {
                    store.t("player.mute")
                }
            },
            move || store.toggle_mute(),
        ))
        .child(
            view().w(px(slider_w)).group().child(
                slider()
                    .value(move || Some(SliderValue::Single(eff_volume())))
                    .min(0.0)
                    .max(1.0)
                    .step(0.02)
                    .accessibility_label(move || Some(store.t("player.volume")))
                    .on_change(move |v: &SliderValue| {
                        let next = v.start() as f32;
                        if store.muted.get_untracked() && next > 0.0 {
                            store.toggle_mute();
                        }
                        store.set_volume(next);
                    })
                    .track_style(move |s| {
                        s.w_full()
                            .h(px(if touch { 44.0 } else { 18.0 }))
                            .justify_center()
                    })
                    .indicator_style(|s| {
                        s.w_full()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(move || theme::np_fg().opacity(0.18))
                            .transition_all()
                            .duration(move || motion::transition_ms(120.0))
                            .group_hover(|g| g.h(px(5.0)))
                    })
                    .range_style(|s| s.rounded_full().bg(move || theme::np_fg().opacity(0.72)))
                    .thumb_style(move |s| {
                        let s = s
                            .size(px(if touch { 14.0 } else { 10.0 }))
                            .rounded_full()
                            .bg(colors::NP_FG);
                        if touch {
                            s.opacity(1.0)
                        } else {
                            s.opacity(0.0).group_hover(|g| g.opacity(1.0))
                        }
                    }),
            ),
        )
}

pub(crate) fn stage_like_button(store: AppStore, glyph_size: f32, touch: bool) -> impl IntoElement {
    let is_liked = computed(move || {
        store
            .current_track
            .with(|t| t.as_ref().is_some_and(|tr| store.is_liked(&tr.id)))
    });
    let pop_scale = animated_value(1.0);
    let prev_liked = stored(untracked(|| is_liked.get()));

    effect(move || {
        let now = is_liked.get();
        if prev_liked.replace(now) != now && now {
            motion::pulse_pop(&pop_scale);
        }
    });

    let target_size = if touch { 44.0 } else { 32.0 };

    button()
        .test_id("np-like-btn")
        .selected(is_liked)
        .accessibility_label(move || {
            Some(if is_liked.get() {
                store.t("player.unlike")
            } else {
                store.t("player.like")
            })
        })
        .on_press(move || store.toggle_like_current())
        .min_w(px(target_size))
        .min_h(px(target_size))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(
            view()
                .pointer_events(PointerEvents::None)
                .bind_scale(&pop_scale)
                .child(icon_reactive(
                    move || {
                        if is_liked.get() {
                            Icon::HeartFilled
                        } else {
                            Icon::Heart
                        }
                    },
                    glyph_size,
                    move || {
                        if is_liked.get() {
                            theme::primary()
                        } else {
                            theme::np_fg().opacity(0.70)
                        }
                    },
                )),
        )
}

fn stage_dismiss_button(store: AppStore, box_size: f32, alpha: f32) -> impl IntoElement {
    let target_size = move || px(box_size.max(theme::detect_device_form().min_touch_target()));

    button()
        .test_id("np-dismiss-btn")
        .accessibility_label(move || Some(store.t("now_playing.minimize")))
        .on_press(move || store.close_now_playing())
        .min_w(target_size)
        .min_h(target_size)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(120.0))
        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(Icon::ChevronDown, 22.0, move || {
            theme::np_fg().opacity(alpha)
        }))
}

fn stage_icon_button(
    glyph: impl IntoReactive<Icon>,
    box_size: f32,
    glyph_size: f32,
    tint: impl IntoReactive<AdaptiveColor>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let label_rx = label.into_reactive();
    let target_size = move || px(box_size.max(theme::detect_device_form().min_touch_target()));

    button()
        .accessibility_label(move || Some(label_rx.get()))
        .on_press(on_press)
        .min_w(target_size)
        .min_h(target_size)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(120.0))
        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, glyph_size, tint))
}

fn stage_toggle_icon(
    glyph: impl IntoReactive<Icon>,
    box_size: f32,
    glyph_size: f32,
    active: impl IntoReactive<bool>,
    active_tint: impl IntoReactive<AdaptiveColor>,
    label: impl IntoReactive<SharedString>,
    on_press: impl Fn() + 'static,
) -> impl IntoElement {
    let active_rx = active.into_reactive();
    let active_tint_rx = active_tint.into_reactive();
    let label_rx = label.into_reactive();
    let active_for_color = active_rx.clone();
    let target_size = move || px(box_size.max(theme::detect_device_form().min_touch_target()));

    button()
        .selected(move || active_rx.get())
        .accessibility_label(move || Some(label_rx.get()))
        .on_press(on_press)
        .min_w(target_size)
        .min_h(target_size)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(colors::TRANSPARENT)
        .transition_all()
        .duration(move || motion::transition_ms(120.0))
        .hover(|s| s.bg(move || theme::np_fg().opacity(0.10)))
        .active(|s| s.scale(motion::press_scale))
        .focus_visible(|s| s.outline(px(2.0)).outline_color(theme::ring))
        .child(icon_reactive(glyph, glyph_size, move || {
            if active_for_color.get() {
                active_tint_rx.get()
            } else {
                theme::np_fg().opacity(0.45)
            }
        }))
}
