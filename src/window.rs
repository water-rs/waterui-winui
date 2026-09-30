//! `waterui::window::Window` → `WinUI` `Window` integration.

use nami::Signal;
use waterui::window::{
    UserAttention, Window as WaterUiWindow, WindowBackground, WindowLevel, WindowState, WindowStyle,
};
use waterui_core::Environment;
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{
    framework, solid_brush, store_event_revoker, store_watcher_guards, subscribe_then_get,
};

/// Opens one `WinUI` `Window` for a `WaterUI` window description and keeps its
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
    crate::theme::attach(&framework(&content), env)?;

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

    install_state_writeback(&window, &desc.state, &desc.level, &content)?;
    install_level(&window, &desc.level, &content);
    install_attention(&window, &desc.attention, &content)?;

    window.Activate()?;
    Ok(window)
}

/// Maximize/restore through the window chrome surfaces as a presenter
/// state change on `AppWindow`; mirror it back into `state`.
fn install_state_writeback(
    window: &Window,
    state: &nami::Binding<WindowState>,
    level: &nami::Computed<WindowLevel>,
    content: &UIElement,
) -> windows_core::Result<()> {
    let state = state.clone();
    let level = level.clone();
    let app_window = app_window(window);
    let changed_app_window = app_window.clone();
    let revoker = app_window.Changed(move |_sender, args| {
        let app_window = &changed_app_window;
        let Ok(args) = args.ok() else {
            return;
        };
        if args.DidPresenterChange().unwrap_or_default()
            && let Ok(overlapped) = app_window
                .Presenter()
                .and_then(|p| p.cast::<OverlappedPresenter>())
        {
            // The level lives on the presenter; a new overlapped
            // presenter (e.g. leaving full screen) needs it reapplied.
            apply_window_level(&overlapped, level.snapshot());
        }
        if !(args.DidPresenterChange().unwrap_or_default()
            || args.DidSizeChange().unwrap_or_default())
        {
            return;
        }
        let Ok(overlapped) = app_window
            .Presenter()
            .and_then(|p| p.cast::<OverlappedPresenter>())
        else {
            return;
        };
        let next = match overlapped.State() {
            Ok(OverlappedPresenterState::Maximized) => WindowState::Maximized,
            Ok(OverlappedPresenterState::Restored) => WindowState::Normal,
            _ => return,
        };
        let current = state.snapshot();
        if matches!(current, WindowState::Normal | WindowState::Maximized) && current != next {
            state.set(next);
        }
    })?;
    store_event_revoker(&framework(content), revoker);
    Ok(())
}

/// Window level → `OverlappedPresenter::IsAlwaysOnTop`. Non-overlapped
/// presenters (full screen) cannot float; nothing is applied there.
fn install_level(window: &Window, level: &nami::Computed<WindowLevel>, content: &UIElement) {
    let queue = window.DispatcherQueue().expect("Window::DispatcherQueue");
    let weak = window.downgrade().expect("Window weak ref");
    let (initial, guard) = subscribe_then_get(level, move |ctx| {
        let level = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(window) = weak.upgrade()
                && let Ok(overlapped) = app_window(&window)
                    .Presenter()
                    .and_then(|p| p.cast::<OverlappedPresenter>())
            {
                apply_window_level(&overlapped, level);
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
    if let Ok(overlapped) = app_window(window)
        .Presenter()
        .and_then(|p| p.cast::<OverlappedPresenter>())
    {
        apply_window_level(&overlapped, initial);
    }
}

/// Attention → `FlashWindowEx`; the window gaining focus settles the
/// request, and `resize_increments` has no Win32 counterpart (it would
/// need `WM_SIZING` filtering, which `WinUI` windows cannot host), so it
/// is ignored.
fn install_attention(
    window: &Window,
    attention: &nami::Binding<Option<UserAttention>>,
    content: &UIElement,
) -> windows_core::Result<()> {
    let queue = window.DispatcherQueue()?;
    let weak = window.downgrade().expect("Window weak ref");
    let (initial, guard) = subscribe_then_get(attention, move |ctx| {
        let request = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(window) = weak.upgrade() {
                apply_attention(&window, request);
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
    apply_attention(window, initial);

    let attention = attention.clone();
    let revoker = window.Activated(move |_sender, _args| {
        attention.set(None);
    })?;
    store_event_revoker(&framework(content), revoker);
    Ok(())
}

/// The content root must be a `Panel` to carry a background brush; when it
/// is not, this reports the backend bug loudly instead of silently dropping
/// the background.
fn root_cast_panel(content: &UIElement) -> windows_core::Result<Panel> {
    content.cast::<Panel>().inspect_err(|_| {
        tracing::error!("window content root is not a Panel; background color cannot apply");
    })
}

fn app_window(window: &Window) -> AppWindow {
    // Presenter-level transitions go through `AppWindow` (on `IWindow2`).
    window
        .cast::<IWindow2>()
        .and_then(|window| window.AppWindow())
        .expect("Window::AppWindow")
}

fn apply_window_state(window: &Window, state: WindowState) {
    match state {
        WindowState::Closed => window.Close().expect("Window::Close"),
        WindowState::Normal
        | WindowState::Minimized
        | WindowState::Maximized
        | WindowState::Fullscreen => {
            let app_window = app_window(window);
            match state {
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
                WindowState::Normal | WindowState::Minimized | WindowState::Maximized => {
                    let overlapped = app_window
                        .Presenter()
                        .and_then(|p| p.cast::<OverlappedPresenter>())
                        .expect("default presenter is OverlappedPresenter");
                    match state {
                        WindowState::Normal => {
                            overlapped.Restore().expect("OverlappedPresenter::Restore");
                        }
                        WindowState::Minimized => {
                            overlapped
                                .Minimize()
                                .expect("OverlappedPresenter::Minimize");
                        }
                        WindowState::Maximized => {
                            overlapped
                                .Maximize()
                                .expect("OverlappedPresenter::Maximize");
                        }
                        WindowState::Closed | WindowState::Fullscreen => {
                            unreachable!("outer match filters both arms")
                        }
                    }
                }
                WindowState::Closed => unreachable!("outer match filters the Closed arm"),
            }
        }
    }
}

fn apply_window_level(overlapped: &OverlappedPresenter, level: WindowLevel) {
    overlapped
        .SetIsAlwaysOnTop(level == WindowLevel::AlwaysOnTop)
        .expect("OverlappedPresenter::SetIsAlwaysOnTop");
}

fn apply_attention(window: &Window, request: Option<UserAttention>) {
    let hwnd = window
        .cast::<IWindowNative>()
        .ok()
        .and_then(|native| {
            let mut hwnd: HWND = core::ptr::null_mut();
            unsafe { native.WindowHandle(&raw mut hwnd) }.ok().ok()?;
            if hwnd.is_null() { None } else { Some(hwnd) }
        })
        .expect("IWindowNative::WindowHandle");
    // Critical flashes caption and taskbar until the window is focused;
    // informational flashes the taskbar button briefly.
    let (flags, count) = match request {
        Some(UserAttention::Critical) => (FLASHW_ALL | FLASHW_TIMERNOFG, 0),
        Some(UserAttention::Informational) => (FLASHW_TRAY, 3),
        None => (FLASHW_STOP, 0),
    };
    let info = FLASHWINFO {
        cbSize: u32::try_from(size_of::<FLASHWINFO>()).expect("FLASHWINFO fits u32"),
        hwnd,
        dwFlags: flags.cast_unsigned(),
        uCount: count,
        dwTimeout: 0,
    };
    unsafe {
        // The BOOL reports only whether the call was scheduled, which a
        // caller cannot act on.
        let _ = FlashWindowEx(&raw const info);
    }
}
