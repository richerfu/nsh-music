//! Adaptive modals (centered `dialog` on Desktop, floating sheet on Pad,
//! bottom `sheet` on Phone), global toast overlay, and styled menus / context
//! menus.

use std::rc::Rc;
use std::time::Duration;

use nsh_base::{
    Anchor, ContextMenuElement, MenuElement, SheetEvent, ToastManager, ToastMotion, ToastOptions,
    ToastStackState, ToastTransitionStatus, alert_dialog, context_menu, dialog, menu, menu_item,
    menu_separator, sheet, toast, toast_stack,
};
use nsh_core::{element::StyleRefinement, prelude::*, time::now};

use super::{
    controls::icon_button_tinted,
    icons::{Icon, icon},
    motion,
    theme::{self, DeviceForm, Elevated, Elevation, colors, font_size, radius},
};

/// Adaptive modal presentation:
/// - **Desktop**: Centered elevated `dialog` with scrim backdrop (`max_w = 500pt`).
/// - **Pad**: Floating rounded dialog sheet (`max_w = 540pt`).
/// - **Phone**: Bottom `sheet` with backdrop/back/close dismissal and safe-area bottom inset.
pub fn adaptive_modal<F, E>(
    open: impl IntoReactive<bool>,
    title: impl IntoReactive<SharedString>,
    on_close: impl Fn() + 'static,
    build_content: F,
) -> View
where
    F: Fn() -> E + 'static,
    E: IntoElement,
{
    let open_rx = open.into_reactive();
    let title_rx = title.into_reactive();
    let on_close: Rc<dyn Fn()> = Rc::new(on_close);
    let content_fn: Rc<dyn Fn() -> AnyElement> = Rc::new(move || build_content().into_element());

    let form = computed(theme::detect_device_form);

    view().child(dynamic(move || {
        let current_form = form.get();
        let open_rx = open_rx.clone();
        let title_rx = title_rx.clone();
        let close = on_close.clone();
        let make_content = content_fn.clone();

        match current_form {
            DeviceForm::Phone => {
                let close_evt = close.clone();
                let close_btn = close.clone();
                let title_a11y = title_rx.clone();
                sheet()
                    .open(move || Some(open_rx.get()))
                    .side(Placement::Bottom)
                    .close_on_swipe(false)
                    .close_on_escape(true)
                    .close_on_backdrop_press(true)
                    .grabber(false)
                    .accessibility_label(move || Some(title_a11y.get()))
                    .on_open_change(move |e: &SheetEvent| {
                        if let SheetEvent::OpenChange(false, _) = e {
                            close_evt();
                        }
                    })
                    .backdrop_style(|s| s.bg(theme::scrim))
                    .popup_style(|s| {
                        s.rounded_t(px(radius::SHEET))
                            .bg(theme::card_elevated)
                            .border_t_1()
                            .border_color(theme::border)
                            .elevation(Elevation::Modal)
                    })
                    .child({
                        let sheet_metrics = || {
                            let win_h = window().height.0;
                            let insets = safe_area();
                            let kb_lift = (keyboard_height().0 - insets.bottom.0).max(0.0);
                            if kb_lift > 0.0 {
                                let sheet_max_h = (win_h - insets.top.0 - kb_lift - 8.0).max(220.0);
                                let scroll_max_h = (sheet_max_h - 84.0).max(120.0);
                                (sheet_max_h, 12.0_f32, scroll_max_h)
                            } else {
                                let sheet_max_h = (win_h * 0.88).max(320.0);
                                let scroll_max_h =
                                    (sheet_max_h - 110.0 - insets.bottom.0).max(220.0);
                                (sheet_max_h, insets.bottom.0 + 20.0, scroll_max_h)
                            }
                        };
                        view()
                            .w_full()
                            .max_h(move || px(sheet_metrics().0))
                            .px_4()
                            .pt_3p5()
                            .pb(move || px(sheet_metrics().1))
                            .gap_3()
                            .child(modal_header(title_rx, close_btn))
                            .child(
                                scroll()
                                    .w_full()
                                    .keyboard_taps(KeyboardTaps::Handled)
                                    .keyboard_dismiss_mode(KeyboardDismissMode::OnDrag)
                                    .max_h(move || px(sheet_metrics().2))
                                    .child(make_content()),
                            )
                    })
                    .into_element()
            }
            DeviceForm::Desktop | DeviceForm::Pad { .. } => {
                let max_w = if current_form.is_desktop() {
                    500.0
                } else {
                    540.0
                };
                when(
                    move || open_rx.get(),
                    move || {
                        let close_cancel = close.clone();
                        let close_bg = close.clone();
                        let close_btn = close.clone();
                        let title_hdr = title_rx.clone();
                        let title_a11y = title_rx.clone();
                        let body = make_content();
                        let card_scale = animated_value(if reduce_motion() { 1.0 } else { 0.96 });
                        let card_y = animated_value(if reduce_motion() { 0.0 } else { 8.0 });
                        if !reduce_motion() {
                            motion::animate_spring(&card_scale, 1.0, motion::spring_snappy());
                            motion::animate_spring(&card_y, 0.0, motion::spring_snappy());
                        }
                        dialog()
                            .animation(if reduce_motion() {
                                ModalAnimation::None
                            } else {
                                ModalAnimation::Fade
                            })
                            .label(move || title_a11y.get())
                            .on_cancel(move || close_cancel())
                            .child(
                                view()
                                    .flex_1()
                                    .w_full()
                                    .h_full()
                                    .items_center()
                                    .justify_center()
                                    .p_6()
                                    .child(
                                        view()
                                            .absolute()
                                            .inset_0()
                                            .bg(theme::scrim)
                                            .focusable(false)
                                            .on_press(move || close_bg()),
                                    )
                                    .child(
                                        Elevated::elevation(
                                            view()
                                                .w_full()
                                                .max_w(px(max_w))
                                                .max_h(move || {
                                                    px((window().height.0 * 0.84).max(320.0))
                                                })
                                                .p_5()
                                                .gap_4()
                                                .rounded(px(radius::XL))
                                                .bg(theme::card_elevated)
                                                .border_1()
                                                .border_color(theme::border)
                                                .bind_scale(&card_scale)
                                                .bind_translate_y(&card_y),
                                            Elevation::Modal,
                                        )
                                        .child(modal_header(title_hdr, close_btn))
                                        .child(
                                            view().w_full().overflow_hidden().child(
                                                scroll()
                                                    .w_full()
                                                    .max_h(move || {
                                                        px((window().height.0 * 0.68).max(220.0))
                                                    })
                                                    .child(body),
                                            ),
                                        ),
                                    ),
                            )
                    },
                )
                .into_element()
            }
        }
    }))
}

/// Shows a native confirmation alert dialog for destructive actions (Delete Playlist, Remove Plugin).
pub fn confirm_destructive(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    cancel_label: impl Into<SharedString>,
    confirm_label: impl Into<SharedString>,
    on_confirm: impl Fn() + 'static,
) {
    let answer = alert_dialog()
        .title(title)
        .description(description)
        .cancel("cancel", cancel_label)
        .destructive("confirm", confirm_label)
        .close_on_escape(true)
        .show();
    spawn(async move {
        if answer
            .await
            .is_ok_and(|res| res.action.as_deref() == Some("confirm"))
        {
            on_confirm();
        }
    });
}

fn modal_header(title: Reactive<SharedString>, on_close: Rc<dyn Fn()>) -> View {
    view()
        .w_full()
        .flex_shrink_0()
        .pb_3()
        .border_b_1()
        .border_color(move || theme::border().opacity(0.6))
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            text(move || title.get())
                .role(AccessibilityRole::Header)
                .flex_1()
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(move || {
                    if theme::detect_device_form().is_phone() {
                        px(17.0)
                    } else {
                        px(font_size::TITLE)
                    }
                })
                .font_weight(FontWeight::BOLD)
                .text_color(theme::fg),
        )
        .child(icon_button_tinted(
            Icon::Close,
            theme::muted_fg,
            "Close",
            move || on_close(),
        ))
}

/// Visual tone for a transient toast notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastKind {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// Payload stored inside [`ToastManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastPayload {
    pub message: SharedString,
    pub kind: ToastKind,
}

/// Copyable handle owning the app-wide [`ToastManager`] lifecycle and visible
/// toast signal.
#[derive(Clone, Copy)]
pub struct ToastController {
    manager: Stored<ToastManager<u64, ToastPayload>>,
    shown: Signal<Vec<(u64, ToastPayload, ToastTransitionStatus)>>,
    state: ToastStackState,
    next_id: Stored<u64>,
    owner: Owner,
    ticking: Stored<bool>,
}

impl Default for ToastController {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastController {
    /// Creates a new [`ToastController`] with an on-demand 50ms lifecycle loop
    /// that sleeps completely (zero timers, zero CPU) whenever no toasts are active.
    pub fn new() -> Self {
        Self {
            manager: stored(ToastManager::new(ToastMotion::sonner())),
            shown: signal(Vec::new()),
            state: ToastStackState::new(),
            next_id: stored(1),
            owner: Owner::current(),
            ticking: stored(false),
        }
    }

    fn ensure_ticking(&self) {
        let already = self.ticking.with(|running| {
            if *running {
                true
            } else {
                *running = true;
                false
            }
        });
        if already {
            return;
        }
        let controller = *self;
        self.owner.run(move || {
            spawn(async move {
                loop {
                    timer(Duration::from_millis(50)).await;
                    controller.tick();
                    if controller.manager.with(|m| m.is_empty()) {
                        controller.ticking.with(|running| *running = false);
                        if !controller.manager.with(|m| m.is_empty()) {
                            controller.ensure_ticking();
                        }
                        break;
                    }
                }
            });
        });
    }

    /// Pushes a toast with default 3.5-second timeout.
    pub fn push(&self, message: impl Into<SharedString>, kind: ToastKind) -> u64 {
        self.push_with_timeout(message, kind, Duration::from_millis(3500))
    }

    /// Pushes a toast with an explicit timeout duration.
    pub fn push_with_timeout(
        &self,
        message: impl Into<SharedString>,
        kind: ToastKind,
        timeout: Duration,
    ) -> u64 {
        let id = self.next_id.with(|n| {
            let cur = *n;
            *n += 1;
            cur
        });
        let payload = ToastPayload {
            message: message.into(),
            kind,
        };
        self.manager.with(|m| {
            m.push(id, payload, ToastOptions::timeout(Some(timeout)), now());
        });
        self.sync();
        self.ensure_ticking();
        id
    }

    pub fn info(&self, message: impl Into<SharedString>) {
        self.push(message, ToastKind::Info);
    }

    pub fn success(&self, message: impl Into<SharedString>) {
        self.push(message, ToastKind::Success);
    }

    pub fn warn(&self, message: impl Into<SharedString>) {
        self.push(message, ToastKind::Warning);
    }

    pub fn error(&self, message: impl Into<SharedString>) {
        self.push(message, ToastKind::Error);
    }

    pub fn dismiss(&self, id: u64) {
        self.manager.with(|m| {
            m.dismiss(&id, now());
        });
        self.sync();
        if !self.manager.with(|m| m.is_empty()) {
            self.ensure_ticking();
        }
    }

    pub fn dismiss_newest(&self) {
        let newest = self.shown.with_untracked(|list| {
            list.iter()
                .rev()
                .find(|(_, _, st)| *st != ToastTransitionStatus::Ending)
                .map(|(id, _, _)| *id)
        });
        if let Some(id) = newest {
            self.dismiss(id);
        }
    }

    fn tick(&self) {
        let changed = self
            .manager
            .with(|m| m.advance(now(), self.state.is_expanded()).changed);
        if changed {
            self.sync();
        }
    }

    fn sync(&self) {
        let items = self.manager.with(|m| {
            m.visible(3)
                .map(|(id, payload, status)| (*id, payload.clone(), status))
                .collect::<Vec<_>>()
        });
        self.shown.set(items);
    }
}

/// Mounts the app-level toast stack overlay in the bottom-right on Desktop/Pad
/// or bottom-center above the tab bar on Phone.
pub fn toast_overlay(controller: ToastController) -> impl IntoElement {
    let placement = move || {
        if theme::detect_device_form().is_phone() {
            Anchor::BottomCenter
        } else {
            Anchor::BottomRight
        }
    };

    overlay()
        .inset_0()
        .pointer_events(PointerEvents::BoxNone)
        .p_4()
        .pb(move || {
            let extra = if theme::detect_device_form().is_phone() {
                84.0
            } else {
                96.0
            };
            px(safe_area().bottom.0 + extra)
        })
        .items_end()
        .justify_end()
        .child(
            toast_stack()
                .placement(placement)
                .state(controller.state)
                .on_cancel(move || controller.dismiss_newest())
                .w(move || {
                    let w = window().width.0;
                    px((w - 32.0).clamp(240.0, 360.0))
                })
                .child(each(
                    controller.shown,
                    |(id, _, _)| *id,
                    move |item, _| {
                        let kind = move || item.with(|(_, p, _)| p.kind);
                        let (glyph, tint) = match item.with_untracked(|(_, p, _)| p.kind) {
                            ToastKind::Info => (Icon::Sparkles, colors::PRIMARY),
                            ToastKind::Success => (Icon::Check, colors::OK),
                            ToastKind::Warning => (Icon::Sparkles, colors::WARN),
                            ToastKind::Error => (Icon::Close, colors::DANGER),
                        };
                        toast()
                            .transition_status(move || item.with(|(_, _, s)| *s))
                            .accessibility_label(move || {
                                Some(item.with(|(_, p, _)| p.message.clone()))
                            })
                            .on_dismiss_request(move || {
                                let id = item.with_untracked(|(id, _, _)| *id);
                                controller.dismiss(id);
                            })
                            .w_full()
                            .flex_row()
                            .items_center()
                            .gap_2p5()
                            .px_3p5()
                            .py_3()
                            .rounded(px(radius::LG))
                            .bg(theme::popover)
                            .border_1()
                            .border_color(move || match kind() {
                                ToastKind::Error => theme::danger(),
                                ToastKind::Warning => theme::warn(),
                                _ => theme::border(),
                            })
                            .elevation(Elevation::Popover)
                            .transition_all()
                            .duration(move || motion::transition_ms(200.0))
                            .starting_class("opacity-0 translate-y-3")
                            .ending_class("opacity-0 scale-95")
                            .child(icon(glyph, 16.0, tint))
                            .child(
                                text(move || item.with(|(_, p, _)| p.message.clone()))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .number_of_lines(2)
                                    .text_size(px(font_size::SUBTITLE))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme::popover_fg),
                            )
                    },
                )),
        )
}

/// Action descriptor for [`styled_context_menu`] and [`styled_menu`].
pub struct ContextMenuAction {
    pub label: SharedString,
    pub icon: Option<Icon>,
    pub destructive: bool,
    pub disabled: bool,
    pub separator_before: bool,
    pub on_select: Rc<dyn Fn()>,
}

impl ContextMenuAction {
    pub fn new(label: impl Into<SharedString>, on_select: impl Fn() + 'static) -> Self {
        Self {
            label: label.into(),
            icon: None,
            destructive: false,
            disabled: false,
            separator_before: false,
            on_select: Rc::new(on_select),
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn destructive(mut self, destructive: bool) -> Self {
        self.destructive = destructive;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn separator_before(mut self) -> Self {
        self.separator_before = true;
        self
    }
}

fn render_menu_actions(actions: Vec<ContextMenuAction>) -> Vec<AnyElement> {
    let is_mobile_os = !os().is_desktop();
    let mut nodes = Vec::with_capacity(actions.len() + 2);
    for act in actions {
        if act.separator_before {
            nodes.push(
                menu_separator()
                    .h(px(1.0))
                    .my_1()
                    .bg(theme::border)
                    .into_element(),
            );
        }
        let cb = act.on_select;
        let tint = if act.destructive {
            colors::DANGER
        } else {
            colors::POPOVER_FOREGROUND
        };
        let label_tint = if act.destructive {
            colors::DANGER
        } else {
            colors::POPOVER_FOREGROUND
        };
        nodes.push(
            menu_item()
                .disabled(act.disabled)
                .on_select(move || cb())
                .min_h(move || {
                    if is_mobile_os || theme::detect_device_form().is_touch() {
                        px(44.0)
                    } else {
                        px(32.0)
                    }
                })
                .px_3()
                .py_1p5()
                .flex_row()
                .items_center()
                .gap_2p5()
                .rounded(px(radius::SM))
                .hover(|s| s.bg(theme::accent))
                .focus(|s| s.bg(theme::accent))
                .disabled_style(|s| s.opacity(0.45))
                .children(act.icon.map(|ic| icon(ic, 15.0, tint)))
                .child(
                    text(act.label)
                        .number_of_lines(1)
                        .text_size(px(font_size::SUBTITLE))
                        .text_color(label_tint),
                )
                .into_element(),
        );
    }
    nodes
}

/// Renders menu actions inside a full-width column view for lazy/dynamic menus.
pub fn render_menu_actions_view(actions: Vec<ContextMenuAction>) -> AnyElement {
    view()
        .w_full()
        .children(render_menu_actions(actions))
        .into_element()
}

fn menu_popup_style(s: StyleRefinement, desktop_min_w: f32) -> StyleRefinement {
    if os().is_desktop() {
        s.min_w(px(desktop_min_w))
            .p_1p5()
            .rounded(px(radius::LG))
            .bg(theme::popover)
            .border_1()
            .border_color(theme::border)
            .elevation(Elevation::Popover)
            .transition_all()
            .duration(move || motion::transition_ms(180.0))
    } else {
        s.rounded_t(px(radius::SHEET))
            .bg(theme::card_elevated)
            .border_t_1()
            .border_color(theme::border)
            .pt_2()
            .px_2()
            .pb(move || px(safe_area().bottom.0 + 12.0))
            .elevation(Elevation::Popover)
    }
}

fn configure_context_menu(trigger: impl IntoElement, desktop_min_w: f32) -> ContextMenuElement {
    let base = context_menu()
        .offset(px(4.0))
        .margin(px(8.0))
        .backdrop_style(|s| s.bg(theme::scrim))
        .popup_style(move |s| menu_popup_style(s, desktop_min_w))
        .trigger(trigger);
    if os().is_desktop() {
        base.popup_starting_class("opacity-0 scale-95")
            .popup_ending_class("opacity-0 scale-95")
    } else {
        base
    }
}

fn configure_menu(trigger: impl IntoElement, desktop_min_w: f32) -> MenuElement {
    let base = menu()
        .offset(px(6.0))
        .margin(px(8.0))
        .backdrop_style(|s| s.bg(theme::scrim))
        .popup_style(move |s| menu_popup_style(s, desktop_min_w))
        .trigger(trigger);
    if os().is_desktop() {
        base.popup_starting_class("opacity-0 scale-95")
            .popup_ending_class("opacity-0 scale-95")
    } else {
        base
    }
}

/// Styled right-click / long-press context menu wrapping `nsh_base::context_menu`.
pub fn styled_context_menu(
    trigger: impl IntoElement,
    actions: Vec<ContextMenuAction>,
) -> ContextMenuElement {
    configure_context_menu(trigger, 196.0).children(render_menu_actions(actions))
}

/// Styled trigger-opened dropdown/sheet menu wrapping `nsh_base::menu`.
pub fn styled_menu(trigger: impl IntoElement, actions: Vec<ContextMenuAction>) -> MenuElement {
    configure_menu(trigger, 196.0).children(render_menu_actions(actions))
}

/// Lazily-built styled context menu whose actions are only materialized when rendered.
pub fn styled_lazy_context_menu<F>(
    trigger: impl IntoElement,
    build_actions: F,
) -> ContextMenuElement
where
    F: Fn() -> Vec<ContextMenuAction> + 'static,
{
    configure_context_menu(trigger, 210.0)
        .child(dynamic(move || render_menu_actions_view(build_actions())))
}

/// Lazily-built styled dropdown/sheet menu whose actions are only materialized when rendered.
pub fn styled_lazy_menu<F>(trigger: impl IntoElement, build_actions: F) -> MenuElement
where
    F: Fn() -> Vec<ContextMenuAction> + 'static,
{
    configure_menu(trigger, 210.0).child(dynamic(move || render_menu_actions_view(build_actions())))
}
