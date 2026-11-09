//! `waterui::window::Window` → `WinUI` `Window` integration.

use std::cell::RefCell;
use std::rc::Rc;

use nami::{Signal, SignalExt as _, signal::IntoComputed as _};
use waterui::window::{
    ResolvedWindowBackground, UserAttention, Window as WaterUiWindow, WindowBackground, WindowIcon,
    WindowLevel, WindowState, WindowStyle, resolve_background,
};
use waterui_controls::menu::ResolvedMenuItem;
use waterui_core::Environment;
use waterui_graphics::color::WorkingColor;
use waterui_layout::Size;
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::executor::{UiThread, enqueue_on_ui_thread};
use crate::renderer::WinUiRenderer;
use crate::session_end::SessionEnd;
use crate::util::{
    framework, solid_brush, store_event_revoker, store_watcher_guards, subscribe_then_get,
};

/// The window a menu is mounted in, as the environment of everything that
/// window renders carries it: closing is a write of [`WindowState::Closed`],
/// the same path a programmatic close takes.
#[derive(Clone)]
pub(crate) struct OwnerWindow {
    state: nami::Binding<WindowState>,
    closable: bool,
}

impl OwnerWindow {
    /// The window `desc` declares.
    fn of(desc: &WaterUiWindow) -> Self {
        Self {
            state: desc.state.clone(),
            closable: desc.closable,
        }
    }

    /// Whether the window was declared with a close button.
    pub(crate) const fn closable(&self) -> bool {
        self.closable
    }

    /// Asks the window to close.
    pub(crate) fn close(&self) {
        self.state.set(WindowState::Closed);
    }
}

/// The colour `WinUI` paints behind a window's content, following
/// `background`.
///
/// `WinUI` realizes no material: a [`WindowBackground::Material`] draws the
/// window opaque in the theme's background colour, as
/// [`WindowBackground::Opaque`] does — the contract of the backends that
/// realize no material.
fn painted_background(
    background: &nami::Binding<WindowBackground>,
    env: &Environment,
) -> nami::Computed<WorkingColor> {
    let without_material = background.map(|background| match background {
        WindowBackground::Material(_) => WindowBackground::Opaque,
        background => background,
    });
    resolve_background(&without_material, env)
        .map(|resolved| match resolved {
            ResolvedWindowBackground::Color(color) => color,
            ResolvedWindowBackground::Material(_) => {
                unreachable!("every material was mapped to an opaque background above")
            }
        })
        .into_computed()
}

/// Opens one `WinUI` `Window` for a `WaterUI` window description and keeps its
/// reactive state (title, open state, background) synchronized.
///
/// The window owns every watcher and event subscription through the root
/// element's attachment bag; the bag dies when the window's `Closed` handler
/// drops the window.
pub fn open_window(
    desc: WaterUiWindow,
    env: &Environment,
    renderer: &mut WinUiRenderer,
    session_end: &SessionEnd,
    menu_bar: &nami::Computed<Vec<ResolvedMenuItem>>,
) -> windows_core::Result<()> {
    let window = Window::new()?;
    let ui = renderer.ui_thread().clone();

    // Everything the window mounts — the menu bar, context menus — sees its
    // owner, so a `MenuItem::CloseWindow` closes the window it sits in.
    let env = &env.extending(OwnerWindow::of(&desc));

    // Content.
    let view = renderer.render_any(desc.content.build(), env);
    let content = window_root(&view, menu_bar, env, &ui)?;
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
        let resolved = painted_background(&desc.background, env);
        let weak = content.downgrade().expect("UIElement weak ref");
        let queue = window.DispatcherQueue()?;
        let (initial, guard) = subscribe_then_get(&resolved, move |ctx| {
            let color = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(root) = weak.upgrade() {
                    root.cast::<Panel>()
                        .expect("window content is a Panel")
                        .SetBackground(&solid_brush(&color).expect("SolidColorBrush"))
                        .expect("Panel::SetBackground");
                }
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

    // Reactive title.
    {
        let ui = ui.clone();
        let weak = window.downgrade().expect("Window weak ref");
        let (initial, guard) = subscribe_then_get(&desc.title, move |ctx| {
            let title = ctx.into_value();
            let weak = weak.clone();
            let ui = ui.clone();
            ui.enqueue(move || {
                if let Some(window) = weak.upgrade() {
                    window.SetTitle(title.as_ref()).expect("Window::SetTitle");
                }
            });
        });
        window.SetTitle(initial.as_ref())?;
        store_watcher_guards(&framework(&content), vec![guard]);
    }

    // The HWND is a window-lifetime fact, not a constructor fact: it does not
    // exist yet for a deferred native window and is gone again once `Close`
    // destroys it. This cell tracks it; the `Activated` subscription is
    // registered before every other one so `HWND`-dependent applications
    // replayed at creation run ahead of handlers like the attention reset.
    let hwnd_state: SharedHwnd = Rc::new(RefCell::new(WindowHwnd {
        hwnd: try_window_hwnd(&window),
        ..WindowHwnd::default()
    }));
    install_hwnd_gate(&window, &content, hwnd_state.clone())?;

    // Logoff and shutdown reach every top-level window as
    // `WM_QUERYENDSESSION`/`WM_ENDSESSION`; the subclass reports them to the
    // termination machine.
    let session_end = session_end.clone();
    hwnd_state
        .borrow_mut()
        .when_created(move |hwnd| session_end.install(hwnd));

    // Close request (user presses X) must mirror into `state`. Under
    // `LastWindowPolicy::Quit` the tracked count reaching zero files a
    // `Required` termination — the `winit` runner's
    // `exit_if_last_window_closed` idiom.
    //
    // The cell below is the window's only Rust-side owner, kept by this
    // handler: `Closed` fires once the native window is gone, so taking
    // the window out of it releases the tree. The drop itself is deferred
    // to the dispatcher — the handler's own subscription lives in that
    // tree and cannot die mid-callback.
    let state = desc.state.clone();
    let closed_hwnd_state = hwnd_state.clone();
    let termination = env.get::<crate::app::WindowTermination>().cloned();
    let owner: Rc<RefCell<Option<Window>>> = Rc::new(RefCell::new(None));
    let closed_owner = owner.clone();
    let ui_for_closed = ui.clone();
    let revoker = window.Closed(move |_sender, _args| {
        closed_hwnd_state.borrow_mut().destroyed();
        state.set(WindowState::Closed);
        if let Some(termination) = &termination {
            termination.closed();
        }
        let closed = closed_owner.borrow_mut().take();
        ui_for_closed.enqueue(move || drop(closed));
    })?;
    store_event_revoker(&framework(&content), revoker);

    install_state(&window, &desc.state, &content, &hwnd_state, &ui);

    install_state_writeback(&window, &desc.state, &desc.level, &content)?;
    install_level(&window, &desc.level, &content, &hwnd_state, &ui);
    install_attention(&window, &desc.attention, &content, &hwnd_state, &ui)?;
    install_icon(&desc.icon, &content, &hwnd_state, &ui);
    install_resize_increments(desc.resize_increments.as_ref(), &content, &hwnd_state, &ui);

    // The `Closed` handler holds the window from here on: nothing else
    // keeps a `Window` reference, so `Closed` emptying the cell ends the
    // whole tree — watchers, revokers and the menu-bar rebuild included.
    *owner.borrow_mut() = Some(window.clone());
    window.Activate()?;
    Ok(())
}

/// Applies programmatic changes of the window's open state while the native
/// window exists, and replays the state the window was declared with once
/// activation creates it.
fn install_state(
    window: &Window,
    state: &nami::Binding<WindowState>,
    content: &UIElement,
    hwnd_state: &SharedHwnd,
    ui: &UiThread,
) {
    let ui = ui.clone();
    let weak = window.downgrade().expect("Window weak ref");
    let hwnd_watcher = hwnd_state.clone();
    let (_, guard) = subscribe_then_get(state, move |ctx| {
        let new_state = ctx.into_value();
        let weak = weak.clone();
        let ui = ui.clone();
        let hwnd_watcher = hwnd_watcher.clone();
        ui.enqueue(move || {
            if let Some(window) = weak.upgrade()
                && hwnd_watcher.borrow().hwnd.is_some()
            {
                apply_window_state(&window, new_state);
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
    let weak = window.downgrade().expect("Window weak ref");
    let state = state.clone();
    hwnd_state.borrow_mut().when_created(move |_hwnd| {
        if let Some(window) = weak.upgrade() {
            let state = state.snapshot();
            if state != WindowState::Normal {
                apply_window_state(&window, state);
            }
        }
    });
}

/// Feeds the live `HWND` into `hwnd_state` once activation creates the
/// native window. Registered before every other `Activated` subscription so
/// `HWND`-dependent applications replayed at creation run ahead of handlers
/// like the attention reset.
fn install_hwnd_gate(
    window: &Window,
    content: &UIElement,
    hwnd_state: SharedHwnd,
) -> windows_core::Result<()> {
    let weak = window.downgrade().expect("Window weak ref");
    let revoker = window.Activated(move |_sender, _args| {
        let Some(hwnd) = weak.upgrade().as_ref().and_then(try_window_hwnd) else {
            return;
        };
        let pending = hwnd_state.borrow_mut().created(hwnd);
        for apply in pending {
            apply(hwnd);
        }
    })?;
    store_event_revoker(&framework(content), revoker);
    Ok(())
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
            Ok(OverlappedPresenterState::Minimized) => WindowState::Minimized,
            Ok(OverlappedPresenterState::Restored) => WindowState::Normal,
            _ => return,
        };
        let current = state.snapshot();
        if matches!(
            current,
            WindowState::Normal
                | WindowState::Minimized
                | WindowState::Maximized
                | WindowState::Fullscreen
        ) && current != next
        {
            state.set(next);
        }
    })?;
    store_event_revoker(&framework(content), revoker);
    Ok(())
}

/// Window level → `OverlappedPresenter::IsAlwaysOnTop`. Non-overlapped
/// presenters (full screen) cannot float; nothing is applied there.
fn install_level(
    window: &Window,
    level: &nami::Computed<WindowLevel>,
    content: &UIElement,
    hwnd_state: &SharedHwnd,
    ui: &UiThread,
) {
    let ui = ui.clone();
    let weak = window.downgrade().expect("Window weak ref");
    let hwnd_watcher = hwnd_state.clone();
    let (_, guard) = subscribe_then_get(level, move |ctx| {
        let level = ctx.into_value();
        let weak = weak.clone();
        let ui = ui.clone();
        let hwnd_watcher = hwnd_watcher.clone();
        ui.enqueue(move || {
            if let Some(window) = weak.upgrade()
                && hwnd_watcher.borrow().hwnd.is_some()
                && let Ok(overlapped) = app_window(&window)
                    .Presenter()
                    .and_then(|p| p.cast::<OverlappedPresenter>())
            {
                apply_window_level(&overlapped, level);
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
    let weak = window.downgrade().expect("Window weak ref");
    let level = level.clone();
    hwnd_state.borrow_mut().when_created(move |_hwnd| {
        if let Some(window) = weak.upgrade()
            && let Ok(overlapped) = app_window(&window)
                .Presenter()
                .and_then(|p| p.cast::<OverlappedPresenter>())
        {
            apply_window_level(&overlapped, level.snapshot());
        }
    });
}

/// Attention → `FlashWindowEx`; the window gaining focus settles the
/// request.
fn install_attention(
    window: &Window,
    attention: &nami::Binding<Option<UserAttention>>,
    content: &UIElement,
    hwnd_state: &SharedHwnd,
    ui: &UiThread,
) -> windows_core::Result<()> {
    let ui = ui.clone();
    let weak = window.downgrade().expect("Window weak ref");
    let hwnd_watcher = hwnd_state.clone();
    let (_, guard) = subscribe_then_get(attention, move |ctx| {
        let request = ctx.into_value();
        let weak = weak.clone();
        let ui = ui.clone();
        let hwnd_watcher = hwnd_watcher.clone();
        ui.enqueue(move || {
            if let Some(window) = weak.upgrade()
                && hwnd_watcher.borrow().hwnd.is_some()
            {
                apply_attention(&window, request);
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
    let weak = window.downgrade().expect("Window weak ref");
    let request = attention.clone();
    hwnd_state.borrow_mut().when_created(move |_hwnd| {
        if let Some(window) = weak.upgrade() {
            apply_attention(&window, request.snapshot());
        }
    });

    let attention = attention.clone();
    let revoker = window.Activated(move |_sender, _args| {
        attention.set(None);
    })?;
    store_event_revoker(&framework(content), revoker);
    Ok(())
}

/// The window's own icon → the `HWND`'s `WM_SETICON` icons, applied while
/// the native window exists and replayed at its creation.
///
/// The window message is the primitive both the caption and the taskbar read,
/// and the one that can give the icon back: a null icon makes the window show
/// its class icon — the application's — again, which `AppWindow.SetIcon` has
/// no documented form for.
fn install_icon(
    icon: &nami::Binding<Option<WindowIcon>>,
    content: &UIElement,
    hwnd_state: &SharedHwnd,
    ui: &UiThread,
) {
    // The icon the window currently shows; the window does not own it, so it
    // is destroyed once a replacement is in place.
    let shown: Rc<RefCell<Option<NativeIcon>>> = Rc::default();
    let ui = ui.clone();
    let hwnd_watcher = hwnd_state.clone();
    let watcher_shown = shown.clone();
    let (_, guard) = subscribe_then_get(icon, move |ctx| {
        let icon = ctx.into_value();
        let ui = ui.clone();
        let hwnd_watcher = hwnd_watcher.clone();
        let shown = watcher_shown.clone();
        ui.enqueue(move || {
            // `WM_SETICON` is synchronous and re-enters the window's
            // subclass, so the shared cell is not borrowed across it.
            let hwnd = hwnd_watcher.borrow().hwnd;
            if let Some(hwnd) = hwnd {
                apply_icon(hwnd, icon.as_ref(), &shown);
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
    let icon = icon.clone();
    hwnd_state.borrow_mut().when_created(move |hwnd| {
        // A window without its own icon already shows the application's.
        if let Some(icon) = icon.snapshot() {
            apply_icon(hwnd, Some(&icon), &shown);
        }
    });
}

/// Puts `icon` — or, for `None`, the class icon — on the window's caption
/// and taskbar entry, then releases the icon it replaced.
fn apply_icon(hwnd: HWND, icon: Option<&WindowIcon>, shown: &RefCell<Option<NativeIcon>>) {
    let native = icon.map(NativeIcon::new);
    let handle = native.as_ref().map_or(0, |native| native.0 as LPARAM);
    for kind in [ICON_SMALL, ICON_BIG] {
        unsafe {
            // The result is the previous icon handle, which `shown` owns.
            SendMessageW(
                hwnd,
                WM_SETICON as u32,
                kind.cast_unsigned() as WPARAM,
                handle,
            );
        }
    }
    *shown.borrow_mut() = native;
}

/// An icon handle this backend created, destroyed with the value.
struct NativeIcon(HICON);

impl NativeIcon {
    /// A 32-bit alpha icon from the straight-alpha RGBA pixels.
    fn new(icon: &WindowIcon) -> Self {
        // GDI takes BGRA rows, top to bottom.
        let (pixels, _) = icon.rgba().as_chunks::<4>();
        let bgra: Vec<u8> = pixels
            .iter()
            .flat_map(|&[red, green, blue, alpha]| [blue, green, red, alpha])
            .collect();
        // The 1-bit AND mask, rows padded to 16 bits. All zero: the colour
        // bitmap's alpha channel alone decides transparency.
        let mask_stride = usize::try_from(icon.width().div_ceil(16) * 2).expect("mask row fits");
        let mask = vec![0_u8; mask_stride * usize::try_from(icon.height()).expect("height fits")];
        let width = i32::try_from(icon.width()).expect("icon width fits an i32");
        let height = i32::try_from(icon.height()).expect("icon height fits an i32");
        let handle = unsafe {
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
        assert!(
            !handle.is_null(),
            "CreateIcon failed for a {width}x{height} window icon: {}",
            std::io::Error::last_os_error()
        );
        Self(handle)
    }
}

impl Drop for NativeIcon {
    fn drop(&mut self) {
        unsafe {
            // A failure leaves nothing to undo.
            let _ = DestroyIcon(self.0);
        }
    }
}

/// Stacks the app's menu bar over the window's content. The `Grid` is the
/// window's root, so the background and the window-lifetime attachments
/// live on it.
fn window_root(
    view: &UIElement,
    menu_bar: &nami::Computed<Vec<ResolvedMenuItem>>,
    env: &Environment,
    ui: &UiThread,
) -> windows_core::Result<UIElement> {
    let root = Grid::new()?;
    let rows = crate::util::vector::<_, RowDefinition>(&root.RowDefinitions()?);
    for unit in [GridUnitType::Auto, GridUnitType::Star] {
        let row = RowDefinition::new()?;
        row.SetHeight(GridLength {
            value: 1.0,
            grid_unit_type: unit,
        })?;
        rows.Append(&row)?;
    }
    let bar = crate::components::menus::build_menu_bar(menu_bar, env, ui);
    Grid::SetRow(&framework(&bar), 0)?;
    Grid::SetRow(&framework(view), 1)?;
    let children = crate::util::children(&root);
    children.Append(&bar)?;
    children.Append(view)?;
    root.cast()
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
    let presenter = app_window(window).Presenter()?;
    if let Ok(overlapped) = presenter.cast::<OverlappedPresenter>() {
        overlapped.SetBorderAndTitleBar(chrome, chrome)?;
    }
    Ok(())
}
fn apply_window_state(window: &Window, state: WindowState) {
    match state {
        WindowState::Closed => window.Close().expect("Window::Close"),
        WindowState::Fullscreen => {
            let app_window = app_window(window);
            let full_screen = FullScreenPresenter::Create().expect("FullScreenPresenter::new");
            app_window
                .SetPresenter(
                    &full_screen
                        .cast::<AppWindowPresenter>()
                        .expect("AppWindowPresenter"),
                )
                .expect("AppWindow::SetPresenter");
        }
        WindowState::Normal | WindowState::Minimized | WindowState::Maximized => {
            let app_window = app_window(window);
            // A non-overlapped presenter (fullscreen) owns the window;
            // switch back to the overlapped presenter before driving it,
            // and leave minimized so maximize/restore can take effect.
            let overlapped = app_window
                .Presenter()
                .and_then(|p| p.cast::<OverlappedPresenter>())
                .unwrap_or_else(|_| {
                    app_window
                        .SetPresenterByKind(AppWindowPresenterKind::Overlapped)
                        .expect("AppWindow::SetPresenterByKind");
                    app_window
                        .Presenter()
                        .and_then(|p| p.cast::<OverlappedPresenter>())
                        .expect("overlapped presenter after SetPresenterByKind")
                });
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
                    if overlapped.State().ok() == Some(OverlappedPresenterState::Minimized) {
                        overlapped.Restore().expect("OverlappedPresenter::Restore");
                    }
                    overlapped
                        .Maximize()
                        .expect("OverlappedPresenter::Maximize");
                }
                WindowState::Closed | WindowState::Fullscreen => {
                    unreachable!("outer match filters both arms")
                }
            }
        }
    }
}

/// `IWindowNative::WindowHandle` reports the `HWND` only while the native
/// window exists, so this returns `None` before activation creates it (a
/// `Window` constructed off-screen owns none yet) and after `Close` destroys
/// it.
fn try_window_hwnd(window: &Window) -> Option<HWND> {
    window.cast::<IWindowNative>().ok().and_then(|native| {
        let mut hwnd: HWND = core::ptr::null_mut();
        unsafe { native.WindowHandle(&raw mut hwnd) }.ok().ok()?;
        if hwnd.is_null() { None } else { Some(hwnd) }
    })
}

fn window_hwnd(window: &Window) -> HWND {
    try_window_hwnd(window).expect("IWindowNative::WindowHandle")
}

/// Shared bookkeeping of the window's `HWND`, used to order `HWND`-dependent
/// applications after the native window's creation. Watchers apply only
/// while `hwnd` is live — before creation their newest value is replayed
/// from the signal at creation, and after destruction there is no window
/// left to apply it to — so no update is silently dropped. Every accessor
/// runs on the UI thread.
#[derive(Default)]
struct WindowHwnd {
    /// The live `HWND`, while the native window exists.
    hwnd: Option<HWND>,
    /// `true` once `Close`/`Closed` destroyed the native window; subclass
    /// data freed on `WM_NCDESTROY` may not be touched after this.
    destroyed: bool,
    /// Applications registered before creation, replayed in order with the
    /// newest signal snapshots once the `HWND` exists.
    pending: Vec<Box<dyn FnOnce(HWND)>>,
}

type SharedHwnd = Rc<RefCell<WindowHwnd>>;

impl WindowHwnd {
    /// Runs `apply` with the `HWND` now, or once activation produces it.
    fn when_created(&mut self, apply: impl FnOnce(HWND) + 'static) {
        if let Some(hwnd) = self.hwnd {
            apply(hwnd);
        } else {
            self.pending.push(Box::new(apply));
        }
    }

    /// Records the live `HWND` and drains the creation queue; already
    /// created or destroyed windows (deactivation events also raise
    /// `Activated`) return an empty queue and keep their state.
    fn created(&mut self, hwnd: HWND) -> Vec<Box<dyn FnOnce(HWND)>> {
        if self.destroyed {
            return Vec::new();
        }
        self.hwnd = Some(hwnd);
        std::mem::take(&mut self.pending)
    }

    /// Forgets the `HWND`: `Window::Close` has destroyed the native window
    /// and applications from now on have nothing to apply to.
    fn destroyed(&mut self) {
        self.hwnd = None;
        self.destroyed = true;
    }
}

/// Latest increment in logical points, written by the computed watcher and
/// read by the subclass proc — both only ever run on the UI thread.
struct ResizeIncrements {
    width: f32,
    height: f32,
}

/// Subclass id for the resize-increment hook; arbitrary but unique per window.
const RESIZE_INCREMENT_SUBCLASS_ID: usize = 0x5755_5249; // "WURI"

/// `resize_increments` → an HWND subclass snapping `WM_SIZING` rects to whole
/// multiples of the increment, measured on the client area in the window's
/// own DPI.
fn install_resize_increments(
    increments: Option<&nami::Computed<Size>>,
    content: &UIElement,
    hwnd_state: &SharedHwnd,
    ui: &UiThread,
) {
    let Some(increments) = increments else {
        return;
    };
    let initial = increments.snapshot();
    // The box backs the subclass's `dwRefData`; allocating it now lets the
    // watcher record increments before the subclass exists.
    let shared = Box::into_raw(Box::new(ResizeIncrements {
        width: initial.width,
        height: initial.height,
    }));
    hwnd_state.borrow_mut().when_created(move |hwnd| {
        let installed = unsafe {
            SetWindowSubclass(
                hwnd,
                Some(resize_increment_subclass),
                RESIZE_INCREMENT_SUBCLASS_ID,
                shared as usize,
            )
        };
        if !installed.as_bool() {
            drop(unsafe { Box::from_raw(shared) });
            panic!("SetWindowSubclass failed");
        }
    });
    let ui = ui.clone();
    let hwnd_watcher = hwnd_state.clone();
    let (_, guard) = subscribe_then_get(increments, move |ctx| {
        let size = ctx.into_value();
        let ui = ui.clone();
        let hwnd_watcher = hwnd_watcher.clone();
        ui.enqueue(move || {
            // SAFETY: `shared` is owned by the subclass until WM_NCDESTROY
            // frees it on this same UI thread — while the window is not yet
            // destroyed the box is either awaiting the subclass or live in
            // it, and writes are legal either way.
            if !hwnd_watcher.borrow().destroyed {
                unsafe {
                    (*shared).width = size.width;
                    (*shared).height = size.height;
                }
            }
        });
    });
    store_watcher_guards(&framework(content), vec![guard]);
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "WMSZ_* edge codes are small positive ints; snapped extents are clamped to i32 by the RECT domain"
)]
unsafe extern "system" fn resize_increment_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    uidsubclass: usize,
    dwrefdata: usize,
) -> LRESULT {
    if msg == WM_SIZING as u32 {
        // SAFETY: `dwrefdata` is the `ResizeIncrements` box handed to
        // `SetWindowSubclass`; it is released on WM_NCDESTROY below, and every
        // access happens on the window's UI thread.
        let increments = unsafe { &*(dwrefdata as *const ResizeIncrements) };
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = f64::from(dpi) / 96.0;
        let step_x = f64::from(increments.width) * scale;
        let step_y = f64::from(increments.height) * scale;
        // SAFETY: `lparam` points at the proposed window `RECT` for the
        // duration of the message; `GetClientRect` writes `client`.
        unsafe {
            let rect = &mut *(lparam as *mut RECT);
            let mut client = RECT::default();
            // The BOOL reports failure only for an invalid window; this HWND
            // is alive while its subclass procs run.
            let _ = GetClientRect(hwnd, &raw mut client);
            // Snap the client extent, not the frame: the WM_SIZING rect
            // includes the non-client border, so subtract it first.
            let frame_w = (rect.right - rect.left) - (client.right - client.left);
            let frame_h = (rect.bottom - rect.top) - (client.bottom - client.top);
            let edge = wparam as i32;
            if step_x > 0.0 {
                let client_w = rect.right - rect.left - frame_w;
                let snapped = ((f64::from(client_w) / step_x).round() * step_x) as i32 + frame_w;
                match edge {
                    WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT => rect.left = rect.right - snapped,
                    _ => rect.right = rect.left + snapped,
                }
            }
            if step_y > 0.0 {
                let client_h = rect.bottom - rect.top - frame_h;
                let snapped = ((f64::from(client_h) / step_y).round() * step_y) as i32 + frame_h;
                match edge {
                    WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT => rect.top = rect.bottom - snapped,
                    _ => rect.bottom = rect.top + snapped,
                }
            }
        }
        return 1;
    }
    if msg == WM_NCDESTROY as u32 {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(resize_increment_subclass), uidsubclass);
            drop(Box::from_raw(dwrefdata as *mut ResizeIncrements));
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn apply_window_level(overlapped: &OverlappedPresenter, level: WindowLevel) {
    overlapped
        .SetIsAlwaysOnTop(level == WindowLevel::AlwaysOnTop)
        .expect("OverlappedPresenter::SetIsAlwaysOnTop");
}

fn apply_attention(window: &Window, request: Option<UserAttention>) {
    let hwnd = window_hwnd(window);
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
