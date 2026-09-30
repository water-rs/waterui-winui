//! `waterui::window::Window` → `WinUI` `Window` integration.

use std::cell::RefCell;
use std::rc::Rc;
use waterui::window::{Window as WaterUiWindow, WindowState, WindowStyle};

use waterui::graphics::peniko::{ImageAlphaType, ImageData, ImageFormat};
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
/// reactive state (title, open state, style, background, icon) synchronized.
///
/// The returned window owns every watcher and event subscription through the
/// root element's attachment bag; keep it alive for the window's lifetime.
pub fn open_window(
    desc: WaterUiWindow,
    env: &Environment,
    renderer: &mut WinUiRenderer,
) -> windows_core::Result<Window> {
    let window = Window::new()?;

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

    // Background: the theme background for an opaque window, the declared
    // colour otherwise, following both a switch between the two and a change
    // of the colour.
    {
        let resolved = waterui::window::resolve_background(&desc.background, env);
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

    // Reactive title bar style.
    {
        let queue = window.DispatcherQueue()?;
        let weak = window.downgrade().expect("Window weak ref");
        let (initial, guard) = subscribe_then_get(&desc.style, move |ctx| {
            let style = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(window) = weak.upgrade() {
                    apply_window_style(&window, style).expect("apply window style");
                }
            });
        });
        apply_window_style(&window, initial)?;
        store_watcher_guards(&framework(&content), vec![guard]);
    }

    // Reactive icon: the window's own when it declares one, the
    // application's otherwise.
    observe_window_icon(&window, &desc.icon, &content)?;

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
        tracing::error!("window content root is not a Panel; background color cannot apply");
    })
}

/// Projects a [`WindowStyle`] onto the window's chrome.
///
/// `Titled` keeps the system title bar and border, `FullSizeContentView`
/// extends the content into the title bar area under the caption buttons, and
/// `Borderless` removes the border and title bar from the overlapped
/// presenter. A full-screen presenter draws no chrome at all, so only the
/// content-extension half applies while the window is full screen.
fn apply_window_style(window: &Window, style: WindowStyle) -> windows_core::Result<()> {
    window.SetExtendsContentIntoTitleBar(style == WindowStyle::FullSizeContentView)?;
    let chrome = style != WindowStyle::Borderless;
    let presenter = window.cast::<IWindow2>()?.AppWindow()?.Presenter()?;
    if let Ok(overlapped) = presenter.cast::<OverlappedPresenter>() {
        overlapped.SetBorderAndTitleBar(chrome, chrome)?;
    }
    Ok(())
}

/// Shows the window's reactive icon and keeps showing its changes, holding
/// the watcher in the content root's attachment bag.
fn observe_window_icon(
    window: &Window,
    icon: &waterui::reactive::Binding<Option<ImageData>>,
    content: &UIElement,
) -> windows_core::Result<()> {
    let queue = window.DispatcherQueue()?;
    let weak = window.downgrade().expect("Window weak ref");
    let shown = Rc::new(RefCell::new(None::<NativeIcon>));
    let (initial, guard) = subscribe_then_get(icon, {
        let shown = shown.clone();
        move |ctx| {
            let icon = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            let shown = shown.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(window) = weak.upgrade() {
                    apply_window_icon(&window, icon.as_ref(), &shown).expect("apply window icon");
                }
            });
        }
    });
    apply_window_icon(window, initial.as_ref(), &shown)?;
    store_watcher_guards(&framework(content), vec![guard]);
    Ok(())
}

/// A Win32 icon built from a window's declared pixels; destroyed once the
/// window shows another one.
struct NativeIcon(HICON);

impl NativeIcon {
    /// Builds a 32-bpp icon — straight-alpha BGRA rows, top to bottom — the
    /// same way winit builds its window icons.
    ///
    /// # Panics
    ///
    /// Panics when the pixels are in a format other than 8-bit RGBA/BGRA or
    /// Win32 cannot create the icon.
    fn new(image: &ImageData) -> Self {
        let swap_red_blue = match image.format {
            ImageFormat::Rgba8 => true,
            ImageFormat::Bgra8 => false,
            other => panic!("WinUI window icon: unsupported pixel format {other:?}"),
        };
        let premultiplied = matches!(image.alpha_type, ImageAlphaType::AlphaPremultiplied);
        let bgra: Vec<u8> = image
            .data
            .data()
            .chunks_exact(4)
            .flat_map(|pixel| {
                let [mut blue, green, mut red, alpha] = [pixel[0], pixel[1], pixel[2], pixel[3]];
                if swap_red_blue {
                    core::mem::swap(&mut red, &mut blue);
                }
                let straight = |channel: u8| {
                    if !premultiplied {
                        return channel;
                    }
                    if alpha == 0 {
                        return 0;
                    }
                    let value =
                        (u16::from(channel) * 255 + u16::from(alpha) / 2) / u16::from(alpha);
                    u8::try_from(value.min(255)).expect("clamped to a byte")
                };
                [straight(blue), straight(green), straight(red), alpha]
            })
            .collect();
        let width = i32::try_from(image.width).expect("icon width fits Win32");
        let height = i32::try_from(image.height).expect("icon height fits Win32");
        // The AND mask is ignored for a 32-bpp icon with alpha, but Win32
        // still reads one: 1 bpp, rows padded to 16 bits.
        let mask_row = usize::try_from(image.width.div_ceil(16) * 2).expect("mask row fits");
        let mask = vec![0_u8; mask_row * usize::try_from(image.height).expect("mask fits")];
        // SAFETY: both buffers outlive the call and hold exactly the bytes a
        // `width` x `height` 1-bpp mask and 32-bpp colour plane take.
        let icon = unsafe {
            CreateIcon(
                core::ptr::null_mut(),
                width,
                height,
                1,
                32,
                mask.as_ptr(),
                bgra.as_ptr(),
            )
        };
        assert!(!icon.is_null(), "WinUI window icon: CreateIcon failed");
        Self(icon)
    }
}

impl Drop for NativeIcon {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the icon `CreateIcon` returned, destroyed once,
        // after the window was given a replacement.
        let destroyed = unsafe { DestroyIcon(self.0) };
        assert!(destroyed.as_bool(), "WinUI window icon: DestroyIcon failed");
    }
}

/// Shows `icon` in the window's title bar and task-bar button, or the
/// application's icon for `None`, keeping the icon alive in `shown` for as
/// long as the window shows it.
fn apply_window_icon(
    window: &Window,
    icon: Option<&ImageData>,
    shown: &RefCell<Option<NativeIcon>>,
) -> windows_core::Result<()> {
    let mut hwnd = core::ptr::null_mut();
    // SAFETY: `hwnd` is a valid out pointer for the window's handle.
    unsafe { window.cast::<IWindowNative>()?.WindowHandle(&raw mut hwnd) }.ok()?;
    let native = icon.map(NativeIcon::new);
    // A null icon puts the window class's — the application's — back.
    let handle = native.as_ref().map_or(core::ptr::null_mut(), |icon| icon.0);
    for size in [ICON_SMALL, ICON_BIG] {
        // SAFETY: `hwnd` is this window's handle and `handle` a live icon or
        // null; `WM_SETICON` only records it.
        unsafe {
            SendMessageW(
                hwnd,
                u32::try_from(WM_SETICON).expect("message id"),
                usize::try_from(size).expect("icon size"),
                handle as isize,
            );
        }
    }
    // The previous icon is destroyed only now, after the window let go of it.
    *shown.borrow_mut() = native;
    Ok(())
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
