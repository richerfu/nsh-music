//! Cross-platform native music player built on Native Shell (NSH).

pub mod pages;
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
    provide_store(store);
    shell::device_shell(store)
}

/// NSH root component named by `nsh.json` (`"root": "App"`).
#[component]
pub fn App() -> impl IntoElement {
    root_app()
}
