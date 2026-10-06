//! `App::run` entry point: bootstrap, UI thread, windows.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use executor_core::{LocalExecutor, Task};
use waterui::app::{
    App, AppParts, LastWindowPolicy, Termination, TerminationHandle, TerminationHost,
    TerminationKind,
};
use waterui_core::Environment;
use windows_core::Interface;

use crate::app_shim::{create_application, install_xaml_controls_resources};
#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::bootstrap::{bootstrap_runtime, initialize_ui_thread};
use crate::executor::{DeferredDispatcherExecutor, UiThread};
use crate::renderer::WinUiRenderer;
use crate::window::open_window;

/// Counts open windows so the last `Closed` can file a `Required`
/// termination — the `winit` runner's `exit_if_last_window_closed` idiom.
/// Every window clones this out of the environment, so the count is shared
/// behind an `Rc`. Installed only under [`LastWindowPolicy::Quit`].
#[derive(Clone)]
pub(crate) struct WindowTermination {
    open: Rc<Cell<usize>>,
    handle: TerminationHandle,
}

impl WindowTermination {
    /// One window closer to empty: the last `Closed` ends the application.
    pub(crate) fn closed(&self) {
        if self.open.get() == 1 {
            self.handle.request(TerminationKind::Required);
        } else {
            self.open.set(self.open.get() - 1);
        }
    }
}

/// Answers the `Termination` machine on behalf of `WinUI`: `terminate`
/// exits the `Application` (the dispatcher loop then unwinds); `refuse` has
/// nothing to undo — quitting holds no veto on `WinUI`.
struct WinUiTerminationHost {
    application: Application,
}

impl TerminationHost for WinUiTerminationHost {
    fn terminate(&self) {
        self.application.Exit().expect("Application::Exit");
    }

    fn refuse(&self) {}
}

/// Wires the app's `Termination` machine and the dispatcher shutdown for
/// the run.
///
/// `start` installs the `Quit` service into `env` and hands back the
/// [`TerminationHandle`] every quit path reports through. `Quit` only
/// holds a `Weak` into the machine, so the runner keeps one handle in
/// `handle_slot` for the life of `Application::Start` — otherwise
/// `Quit::request` (the Exit item, a `|quit: Quit|` handler) would panic
/// once startup's copy dropped. [`WindowTermination`] carries a second
/// clone under [`LastWindowPolicy::Quit`] to count window closes.
///
/// `WinUI`'s own `OnLastWindowClose` would end the dispatcher without
/// running `on_terminate`, so shutdown is always explicit and the last
/// window's `Closed` files through the machine instead.
///
/// Returns `false` when the run should end immediately: a windowless app
/// under `Quit` files its `Required` request here and opens nothing.
/// Session end (`WM_QUERYENDSESSION`/`WM_ENDSESSION`) and console control
/// events are not wired.
fn wire_termination(
    env: &mut Environment,
    termination: Termination,
    application: &Application,
    window_count: usize,
    last_window: LastWindowPolicy,
    handle_slot: &Rc<OnceCell<TerminationHandle>>,
) -> bool {
    application
        .cast::<IApplication3>()
        .expect("Application is an IApplication3")
        .SetDispatcherShutdownMode(DispatcherShutdownMode::OnExplicitShutdown)
        .expect("Application::SetDispatcherShutdownMode");

    let handle = termination.start(
        env,
        WinUiTerminationHost {
            application: application.clone(),
        },
    );
    // The slot clone owns the machine for the whole run under every
    // policy — under `StayResident` no `WindowTermination` exists to keep
    // `Quit`'s target alive — and drops when `run_app` unwinds after
    // `Application::Start` returns.
    handle_slot
        .set(handle.clone())
        .map_err(|_| ())
        .expect("wire_termination ran twice");

    if last_window != LastWindowPolicy::Quit {
        return true;
    }
    if window_count == 0 {
        // No window will ever close, so nothing else would end the
        // dispatcher: an app that quits after its last window and declares
        // none exits at launch.
        tracing::info!("the application declares no window and quits after its last one");
        handle.request(TerminationKind::Required);
        return false;
    }
    env.insert(WindowTermination {
        open: Rc::new(Cell::new(window_count)),
        handle,
    });
    true
}

/// Runs a `WaterUI` application on the `WinUI` backend.
///
/// Bootstraps the Windows App Runtime, initializes a per-monitor-aware STA UI
/// thread, starts the `WinUI` `Application` message loop (blocking), and opens
/// the app's windows inside `OnLaunched`.
///
/// The app may declare no window. Its [`LastWindowPolicy`] decides what
/// happens once none is open: under [`LastWindowPolicy::Quit`] the last
/// window's `Closed` files a `Required` request through the app's
/// `Termination` machine — `WinUI`'s own `OnLastWindowClose` shutdown is
/// disabled because it would skip `on_terminate` — and at launch a
/// windowless app files the same request; under
/// [`LastWindowPolicy::StayResident`] the dispatcher also shuts down only on
/// an explicit exit.
///
/// # Errors
///
/// Returns an error when the Windows App Runtime cannot be bootstrapped, the
/// UI thread cannot be initialized, or the `WinUI` application fails to start.
///
/// # Panics
///
/// Panics when called off the process's main thread or when the Windows App
/// Runtime cannot be initialized.
///
/// The application is passed as a factory, not a value: building an `App` may
/// already spawn executor work (`waterui-browser-cef::install` starts a
/// message pump task), so the dispatcher-backed local executor must exist on
/// this thread before `make_app` runs.
#[expect(
    clippy::too_many_lines,
    reason = "the run is one linear sequence — bootstrap, executor install, the Start callback, exception surface, GPU, theme, termination, then opening every window — kept flat like the other runners"
)]
pub fn run_app(make_app: impl FnOnce() -> App) -> windows_core::Result<()> {
    // Diagnostics land on stderr so harnesses capture them; `try_init` never
    // overrides a consumer-installed subscriber.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();

    bootstrap_runtime()?;
    initialize_ui_thread()?;

    // View bodies and install hooks spawn tasks through executor-core's
    // thread-local and global slots (`.task(...)`, `spawn_local`, `spawn`).
    // `Application::Start` owns creation of the UI thread's `DispatcherQueue`,
    // so the local slot gets a `DeferredDispatcherExecutor`: schedules made
    // while `make_app` runs (e.g. `waterui_browser_cef::install` starting the
    // CEF message pump) are buffered and flushed onto the dispatcher when the
    // `Start` callback activates it below.
    let deferred = DeferredDispatcherExecutor::new();
    let _ = executor_core::try_init_global_executor(native_executor::NativeExecutor::new());
    let _ = executor_core::try_init_local_executor(
        // The executor's frame budget paces at the headless rate: at app start
        // no swap chain exists yet to ask DXGI for the monitor's real rate,
        // and `CompositionTarget::Rendering` vsyncs the renderer anyway.
        waterui::task::monitored_local_executor_with_probes(
            deferred.clone(),
            waterui::task::RefreshRate::HEADLESS,
            None::<std::sync::Arc<dyn waterui::task::RuntimeProbe>>,
        ),
    );

    let AppParts {
        windows,
        env,
        last_window,
        termination,
        ..
    } = make_app().into_parts();
    let windows = RefCell::new(Some(windows));
    let env = RefCell::new(Some(env));
    let termination = RefCell::new(Some(termination));
    // One `TerminationHandle` the startup task fills in; `Application::Start`
    // blocks for the whole run, so this `Rc` keeps the machine alive until
    // the message loop has already unwound.
    let handle_slot = Rc::new(OnceCell::<TerminationHandle>::new());

    let result = Application::Start(&ApplicationInitializationCallback::new({
        let handle_slot = handle_slot.clone();
        move |_params| {
            // The UI thread's `DispatcherQueue` now exists: bind it to the
            // executor installed before `make_app` and replay buffered schedules.
            deferred
                .activate()
                .expect("UI thread DispatcherQueue missing inside Application::Start");
            let windows = windows
                .borrow_mut()
                .take()
                .expect("Application::Start callback ran twice");
            let env = env
                .borrow_mut()
                .take()
                .expect("Application::Start callback ran twice");
            let termination = termination
                .borrow_mut()
                .take()
                .expect("Application::Start callback ran twice");

            let handle_slot = handle_slot.clone();
            let _app = create_application(Box::new(move || {
                let application = Application::Current().expect("Application::Current");
                install_xaml_controls_resources(&application)?;

                // WinRT stowed exceptions (0xC000027B) carry no stderr output of
                // their own; surface the real HRESULT and message here. `Handled`
                // is left unset — the exception still terminates the process.
                let revoker = application
                    .UnhandledException(|_sender, args| {
                        if let Ok(args) = args.ok() {
                            let code = args.Exception().unwrap_or_default();
                            let message = args.Message().unwrap_or_default();
                            tracing::error!(?code, %message, "unhandled XAML exception");
                        }
                    })
                    .expect("Application::UnhandledException");
                // The subscription lives for the process lifetime.
                core::mem::forget(revoker);

                waterui_locale::start_system_locale_listener();
                let mut renderer = WinUiRenderer::new(UiThread::for_current_thread()?);
                let executor = renderer.executor().clone();

                // GPU-backed content (`GpuContentView`, `FilteredView`) shares
                // one device, created on the dispatcher and installed into the
                // environment before any window content is rendered.
                let pending = Rc::new(RefCell::new(windows));
                let handle_slot = handle_slot.clone();
                executor
                    .spawn_local(async move {
                        let mut env = env;
                        #[cfg(feature = "gpu")]
                        {
                            let runtime =
                                waterui_graphics::GpuRuntime::new()
                                    .await
                                    .unwrap_or_else(|error| {
                                        panic!("WinUI GPU runtime creation failed: {error}")
                                    });
                            env.insert(runtime);
                        }
                        // Theme tokens (fonts, palette, color scheme) must exist
                        // before any view resolves.
                        crate::theme::install(&mut env).expect("WinUI theme installation failed");
                        // The `Termination` machine starts on the fully assembled
                        // environment so its hooks' extractors see theme and GPU
                        // state, and installs `Quit` before any window or menu
                        // can file through it.
                        let window_count = pending.borrow().len();
                        if !wire_termination(
                            &mut env,
                            termination,
                            &application,
                            window_count,
                            last_window,
                            &handle_slot,
                        ) {
                            return;
                        }
                        let windows = pending.borrow_mut().drain(..).collect::<Vec<_>>();
                        for desc in windows {
                            let window = open_window(desc, &env, &mut renderer)
                                .expect("failed to open WaterUI window");
                            // WinUI keeps a window alive until it closes; the
                            // leaked vector pins them for the process lifetime.
                            let _leaked: &'static _ = Box::leak(Box::new(window));
                        }
                    })
                    .detach();
                Ok(())
            }))
            .expect("Application::compose");
        }
    }));

    // `Application::Start` returns once the dispatcher shuts down; the slot —
    // and with it the machine — drops here, after the run has ended.
    result
}
