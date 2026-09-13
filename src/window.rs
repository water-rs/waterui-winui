//! `waterui::window::Window` → WinUI `Window` integration.

use waterui::window::{Window as WaterUiWindow, WindowBackground, WindowState, WindowStyle};
use waterui_core::Environment;
use windows_core::Interface;

use crate::bindings::*;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{
    framework, solid_brush, store_event_revoker, store_watcher_guards, subscribe_then_get,
};

/// Opens one WinUI `Window` for a `WaterUI` window description and keeps its
/// reactive state (title, open state, background) synchronized.
///
/// The returned window owns every watcher and event subscription through the
/// root element's attachment bag; keep it alive for the window's lifetime.
pub fn open_window(
    desc: WaterUiWindow,
    env: &Environment,
    renderer: &mut WinUiRenderer,
) -> windows_core::Result<Window> {
    let window = Window::new()?;

    // Title bar style.
    match desc.style {
        WindowStyle::Titled => {}
        WindowStyle::Borderless | WindowStyle::FullSizeContentView => {
            window.SetExtendsContentIntoTitleBar(true)?;
        }
    }

    // Content.
    let content = renderer.render_any(desc.content.build(), env);
    window.SetContent(&content)?;

    // Toolbar: WinUI exposes a custom title-bar element; when the window
    // declares a toolbar, host it in the title bar area.
    if let Some(toolbar) = desc.toolbar {
        let toolbar_element = renderer.render_any(toolbar, env);
        window.SetTitleBar(&toolbar_element)?;
    }

    // Background.
    match &desc.background {
        WindowBackground::Opaque => {}
        WindowBackground::Color(color) => {
            let resolved = color.resolve(env);
            let root = content.clone();
            let queue = window.DispatcherQueue()?;
            let (initial, guard) = subscribe_then_get(&resolved, move |ctx| {
                let color = ctx.into_value();
                let root = root.clone();
                let queue = queue.clone();
                enqueue_on_ui_thread(&queue, move || {
                    root.cast::<Panel>()
                        .expect("window content is a Panel")
                        .SetBackground(&solid_brush(&color).expect("SolidColorBrush"))
                        .expect("Panel::SetBackground");
                });
            });
            root_cast_panel(&content)?
                .SetBackground(&solid_brush(&initial).expect("SolidColorBrush"))?;
            store_watcher_guards(&framework(&content), vec![guard]);
        }
    }

    // Reactive title.
    {
        let queue = window.DispatcherQueue()?;
        let weak = window.downgrade().expect("Window weak ref");
        let (initial, guard) = subscribe_then_get(&desc.title, move |ctx| {
            let title = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(window) = weak.upgrade() {
                    window.SetTitle(title.as_ref()).expect("Window::SetTitle");
                }
            });
        });
        window.SetTitle(initial.as_ref())?;
        store_watcher_guards(&framework(&content), vec![guard]);
    }

    // Close request (user presses X) must mirror into `state`.
    let state = desc.state.clone();
    let revoker = window.Closed(move |_sender, _args| {
        state.set(WindowState::Closed);
    })?;
    store_event_revoker(&framework(&content), revoker);

    // React to programmatic state changes.
    {
        let queue = window.DispatcherQueue()?;
        let weak = window.downgrade().expect("Window weak ref");
        let (initial, guard) = subscribe_then_get(&desc.state, move |ctx| {
            let new_state = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(window) = weak.upgrade() {
                    apply_window_state(&window, new_state);
                }
            });
        });
        store_watcher_guards(&framework(&content), vec![guard]);
        if initial != WindowState::Normal {
            apply_window_state(&window, initial);
        }
    }

    window.Activate()?;
    Ok(window)
}

/// The content root must be a `Panel` to carry a background brush; when it
/// is not, this reports the backend bug loudly instead of silently dropping
/// the background.
fn root_cast_panel(content: &UIElement) -> windows_core::Result<Panel> {
    content.cast::<Panel>().inspect_err(|_| {
        tracing::error!("window content root is not a Panel; background color cannot apply")
    })
}

fn apply_window_state(window: &Window, state: WindowState) {
    match state {
        WindowState::Closed => window.Close().expect("Window::Close"),
        WindowState::Normal | WindowState::Minimized | WindowState::Fullscreen => {
            // Presenter-level transitions go through `AppWindow` (on `IWindow2`).
            let app_window = window
                .cast::<IWindow2>()
                .and_then(|window| window.AppWindow())
                .expect("Window::AppWindow");
            let overlapped = app_window
                .Presenter()
                .and_then(|p| p.cast::<OverlappedPresenter>())
                .expect("default presenter is OverlappedPresenter");
            match state {
                WindowState::Normal => overlapped.Restore().expect("OverlappedPresenter::Restore"),
                WindowState::Minimized => overlapped
                    .Minimize()
                    .expect("OverlappedPresenter::Minimize"),
                WindowState::Fullscreen => {
                    let full_screen =
                        FullScreenPresenter::Create().expect("FullScreenPresenter::new");
                    app_window
                        .SetPresenter(
                            &full_screen
                                .cast::<AppWindowPresenter>()
                                .expect("AppWindowPresenter"),
                        )
                        .expect("AppWindow::SetPresenter");
                }
                WindowState::Closed => unreachable!("outer match filters the Closed arm"),
            }
        }
    }
}
