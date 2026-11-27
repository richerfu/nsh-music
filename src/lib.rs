//! Cross-platform native music player built on Native Shell (NSH).

pub mod pages;
pub use nsh_music_platforms as platform;
pub mod shell;
pub mod state;
pub mod ui;

use nsh_core::prelude::*;

use crate::state::{AppStore, provide_store};

/// NSH native module registration (`nsh.json` `"rust": "."`).
#[native_module("nsh-music/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// Loads the bundled `theme.json` design tokens for standalone and test runners.
pub fn app_tokens() -> Tokens {
    nsh_base::Theme::from_json(include_str!("../theme.json"))
        .expect("nsh-music theme.json is valid")
        .into_tokens()
}

/// Mounts the full adaptive `nsh-music` application root with a fresh [`AppStore`].
pub fn root_app() -> View {
    root_app_with_store(AppStore::new())
}

/// Mounts the full adaptive `nsh-music` application root bound to a caller-provided [`AppStore`].
pub fn root_app_with_store(store: AppStore) -> View {
    use std::rc::Rc;

    use nsh_core::{
        desktop::WindowKind,
        element::desktop::{WindowRef, current_window, on_reopen, window as desktop_window},
    };

    provide_store(store);

    if !os().is_desktop() {
        return view()
            .flex_1()
            .w_full()
            .h_full()
            .child(shell::device_shell(store));
    }

    let main_window_ref: Stored<Option<WindowRef>> = stored(None);
    let show_main: Rc<dyn Fn()> = Rc::new(move || {
        if !store.main_window_open.get_untracked() {
            store.open_main_window();
        } else if let Some(win) = main_window_ref.with(|w| w.clone()) {
            win.focus();
        }
    });

    let reopen = show_main.clone();
    on_reopen(move || reopen());
    shell::desktop::install_desktop_menu_and_tray(store, show_main);

    let main_win = desktop_window()
        .id("main")
        .kind(WindowKind::Normal)
        .restore_bounds(true)
        .title(move || store.t("app.name").to_string())
        .attributes(shell::desktop::main_window_attributes())
        .open(store.main_window_open)
        .on_open_change(move |e| {
            if !e.open {
                main_window_ref.with(|w| *w = None);
                store.close_main_window();
            }
        })
        .content(move || {
            main_window_ref.with(|w| *w = Some(current_window()));
            shell::device_shell(store)
        });

    view()
        .flex_1()
        .w_full()
        .h_full()
        .child(main_win)
        .child(shell::desktop::settings_window(store))
}

/// NSH root component named by `nsh.json` (`"root": "App"`).
#[component]
pub fn App() -> impl IntoElement {
    root_app()
}
