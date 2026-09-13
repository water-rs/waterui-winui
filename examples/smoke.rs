//! Smoke test for `waterui-winui`: opens a real WinUI window, verifies that the
//! content view renders (the `Appear` lifecycle hook is the readiness signal),
//! then closes the window through `WindowState::Closed` so the process exits
//! with a successful status.
//!
//! Run on Windows with `cargo run --example smoke`. CI runs it on
//! `windows-latest` against the real Windows App SDK runtime.

fn main() {
    #[cfg(target_os = "windows")]
    run();
    #[cfg(not(target_os = "windows"))]
    panic!("waterui-winui only runs on Windows");
}

#[cfg(target_os = "windows")]
fn run() {
    use waterui::app::App;
    use waterui::env::Environment;
    use waterui::prelude::*;
    use waterui::window::{Window, WindowState};

    let state = binding(WindowState::Normal);
    let closer = state.clone();
    let window = Window::new("waterui-winui smoke", state, move || {
        text("WaterUI on WinUI — smoke test").on_appear(move || {
            closer.set(WindowState::Closed);
        })
    });
    waterui_winui::run_app(App::new_with_windows([window], Environment::new()));
}
