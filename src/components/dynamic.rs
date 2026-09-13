//! `Dynamic` — a host that swaps its content when the watched view changes.

use waterui_core::dynamic::Dynamic;
use waterui_core::{Environment, Native};
use windows_core::Interface;

use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;

impl WinUiComponent for Native<Dynamic> {
    /// Renders into a `Border`; each update re-renders the child on the UI
    /// thread with a fresh renderer.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let dynamic = self.into_inner();
        let host = Border::new().expect("Border::new");

        let queue = renderer.executor().queue().clone();
        let env = env.clone();
        let weak = host.downgrade().expect("Border supports weak references");

        dynamic.connect(move |ctx| {
            let view = ctx.into_value();
            let env = env.clone();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                let Some(host) = weak.upgrade() else {
                    return;
                };
                let mut renderer =
                    WinUiRenderer::for_current_thread().expect("renderer on the UI thread");
                let child = renderer.render_any(view, &env);
                host.SetChild(&child).expect("Border::SetChild");
            });
        });

        host.cast().expect("Border is a UIElement")
    }
}
