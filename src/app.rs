//! `App::run` entry point: bootstrap, UI thread, windows.

use std::cell::RefCell;
use std::rc::Rc;

use executor_core::{LocalExecutor, Task};
use waterui::app::App;

use crate::app_shim::{create_application, install_xaml_controls_resources};
use crate::bindings::*;
use crate::bootstrap::{bootstrap_runtime, initialize_ui_thread};
use crate::executor::DispatcherQueueExecutor;
use crate::renderer::WinUiRenderer;
use crate::window::open_window;

/// Runs a `WaterUI` application on the WinUI backend.
///
/// Bootstraps the Windows App Runtime, initializes a per-monitor-aware STA UI
/// thread, starts the WinUI `Application` message loop (blocking), and opens
/// the app's windows inside `OnLaunched`.
///
/// # Panics
///
/// Panics when called off the process's main thread or when the Windows App
/// Runtime cannot be initialized.
pub fn run_app(app: App) -> windows_core::Result<()> {
    bootstrap_runtime()?;
    initialize_ui_thread()?;

    let (windows, _menu_bar, env) = app.into_parts();
    let windows = RefCell::new(Some(windows));
    let env = RefCell::new(Some(env));

    Application::Start(&ApplicationInitializationCallback::new(move |_params| {
        let windows = windows
            .borrow_mut()
            .take()
            .expect("Application::Start callback ran twice");
        let env = env
            .borrow_mut()
            .take()
            .expect("Application::Start callback ran twice");

        let _app = create_application(Box::new(move || {
            let application = Application::Current().expect("Application::Current");
            install_xaml_controls_resources(&application)?;

            let executor = DispatcherQueueExecutor::for_current_thread()?;
            let mut renderer = WinUiRenderer::new(executor.clone());

            // GPU-backed surfaces (GpuSurface, AppliedFilter, vector scenes)
            // share one device, created on the dispatcher and installed into
            // the environment before any window content is rendered.
            let pending = Rc::new(RefCell::new(windows));
            executor
                .spawn_local(async move {
                    #[cfg(feature = "gpu")]
                    let env = {
                        let mut env = env;
                        let runtime =
                            waterui_graphics::GpuRuntime::new()
                                .await
                                .unwrap_or_else(|error| {
                                    panic!("WinUI GPU runtime creation failed: {error}")
                                });
                        env.insert(runtime);
                        env
                    };
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
    }))
}
