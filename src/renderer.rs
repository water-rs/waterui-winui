//! View renderer that dispatches `WaterUI` views to WinUI elements.

use waterui::component::list::ListConfig;
use waterui::component::progress::ProgressConfig;
use waterui::prelude::Divider;
use waterui_backend_core::ViewDispatcher;
use waterui_controls::button::ButtonConfig;
use waterui_controls::menu::ResolvedMenu;
use waterui_controls::slider::SliderConfig;
use waterui_controls::stepper::StepperConfig;
use waterui_controls::text_field::ResolvedTextFieldConfig;
use waterui_controls::toggle::ToggleConfig;
use waterui_core::dynamic::Dynamic;
use waterui_core::metadata::MetadataKey;
use waterui_core::{AnyView, Environment, Metadata, Native, Str, View};
use waterui_form::picker::PickerConfig;
use waterui_form::picker::color::ColorPickerConfig;
use waterui_form::picker::date::DatePickerConfig;
use waterui_form::picker::multi_date::MultiDatePickerConfig;
use waterui_form::secure::SecureFieldConfig;
#[cfg(feature = "gpu")]
use waterui_graphics::ResolvedGradient;
use waterui_graphics::color::{Color, ResolvedColor};
#[cfg(feature = "gpu")]
use waterui_graphics::gpu_surface::GpuSurface;
use waterui_icon::SystemIcon;
use waterui_layout::container::{FixedContainer, LazyContainer};
use waterui_layout::scroll::ScrollView;
use waterui_layout::spacer::Spacer;
use waterui_navigation::tab::TabsLayout;
use waterui_navigation::{NavigationSplitLayout, NavigationStack, NavigationView};
use waterui_shape::ResolvedShape;
use waterui_text::TextConfig;
use windows_core::Interface;

use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::DispatcherQueueExecutor;

/// Context passed to component handlers during dispatch.
///
/// Only [`WinUiRenderer::render`] and [`WinUiRenderer::render_any`] construct
/// this, always from a live `&mut WinUiRenderer` immediately before a
/// synchronous dispatch, so the pointer is never null and outlives every
/// handler invocation it is passed to.
#[derive(Debug, Clone)]
pub struct RenderContext {
    renderer_ptr: *mut WinUiRenderer,
}

impl RenderContext {
    const fn with_renderer(renderer: &mut WinUiRenderer) -> Self {
        Self {
            renderer_ptr: std::ptr::from_mut::<WinUiRenderer>(renderer),
        }
    }

    /// Gets a mutable reference to the renderer.
    ///
    /// # Safety
    /// The caller must uphold that this context was created for the dispatch
    /// currently running on the UI thread.
    #[allow(
        clippy::mut_from_ref,
        reason = "RenderContext is a raw-pointer handle threaded through dispatch; \
                  the caller upholds exclusivity, which the signature cannot express"
    )]
    pub(crate) unsafe fn renderer(&self) -> &mut WinUiRenderer {
        // SAFETY: upheld by the dispatch contract documented on the type.
        unsafe { &mut *self.renderer_ptr }
    }
}

/// Renders `WaterUI` views into native WinUI `UIElement`s.
pub struct WinUiRenderer {
    dispatcher: ViewDispatcher<(), RenderContext, UIElement>,
    executor: DispatcherQueueExecutor,
}

impl WinUiRenderer {
    /// Creates a renderer bound to the current thread's `DispatcherQueue`.
    ///
    /// Must be called on the UI thread after the runtime is bootstrapped.
    pub fn for_current_thread() -> windows_core::Result<Self> {
        Ok(Self::new(DispatcherQueueExecutor::for_current_thread()?))
    }

    /// Creates a renderer on an explicit executor.
    #[must_use]
    pub fn new(executor: DispatcherQueueExecutor) -> Self {
        let mut dispatcher = ViewDispatcher::new();
        Self::register_components(&mut dispatcher);
        Self {
            dispatcher,
            executor,
        }
    }

    /// The executor that marshals work onto the UI thread.
    pub(crate) fn executor(&self) -> &DispatcherQueueExecutor {
        &self.executor
    }

    /// Renders a view to a WinUI element.
    pub fn render<V: View>(&mut self, view: V, env: &Environment) -> UIElement {
        let ctx = RenderContext::with_renderer(self);
        self.dispatcher.dispatch(view, env, ctx)
    }

    /// Renders an `AnyView` to a WinUI element.
    pub fn render_any(&mut self, view: AnyView, env: &Environment) -> UIElement {
        let ctx = RenderContext::with_renderer(self);
        self.dispatcher.dispatch(view, env, ctx)
    }

    fn register_components(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
        Self::register_native::<TextConfig>(dispatcher);
        Self::register_native::<Spacer>(dispatcher);
        Self::register_native::<FixedContainer>(dispatcher);
        Self::register_native::<LazyContainer>(dispatcher);
        Self::register_native::<ButtonConfig>(dispatcher);
        Self::register_native::<ToggleConfig>(dispatcher);
        Self::register_native::<SliderConfig>(dispatcher);
        Self::register_native::<ResolvedTextFieldConfig>(dispatcher);
        Self::register_native::<ProgressConfig>(dispatcher);
        Self::register_native::<StepperConfig>(dispatcher);
        Self::register_native::<ScrollView>(dispatcher);
        Self::register::<TabsLayout>(dispatcher);
        Self::register_native::<ListConfig>(dispatcher);
        Self::register_native::<SecureFieldConfig>(dispatcher);
        Self::register_native::<PickerConfig>(dispatcher);
        Self::register_native::<DatePickerConfig>(dispatcher);
        Self::register_native::<MultiDatePickerConfig>(dispatcher);
        Self::register_native::<ColorPickerConfig>(dispatcher);
        Self::register_native::<ResolvedMenu>(dispatcher);
        Self::register_native::<SystemIcon>(dispatcher);
        Self::register_native::<Color>(dispatcher);
        Self::register_native::<ResolvedColor>(dispatcher);
        #[cfg(feature = "gpu")]
        Self::register_native::<ResolvedGradient>(dispatcher);
        Self::register_native::<ResolvedShape>(dispatcher);
        #[cfg(feature = "gpu")]
        Self::register_native::<GpuSurface>(dispatcher);

        Self::register::<Native<Dynamic>>(dispatcher);

        Self::register::<Divider>(dispatcher);
        Self::register::<NavigationView>(dispatcher);
        Self::register::<NavigationStack<(), ()>>(dispatcher);
        Self::register::<NavigationSplitLayout>(dispatcher);

        Self::register_metadata_handlers(dispatcher);
        Self::register_str_handler(dispatcher);
        Self::register_unit_handler(dispatcher);
    }

    fn register_str_handler(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
        dispatcher.register::<Str>(|_state, _ctx, s, _env| {
            let block = TextBlock::new().expect("TextBlock::new");
            block.SetText(s.as_str()).expect("TextBlock::SetText");
            block.cast().expect("TextBlock is a UIElement")
        });
    }

    fn register_unit_handler(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
        dispatcher.register::<Native<()>>(|_state, _ctx, _unit, _env| {
            let panel = crate::layout::empty_panel().expect("empty panel");
            let element: UIElement = panel.cast().expect("Panel is a UIElement");
            element
                .SetVisibility(Visibility::Collapsed)
                .expect("UIElement::SetVisibility");
            element
        });
    }

    fn register_metadata_handlers(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
        crate::components::metadata::register(dispatcher);
    }

    /// Registers a handler for `Metadata<T>` that renders the content
    /// unchanged because the metadata has no WinUI realization.
    pub(crate) fn register_passthrough_metadata<T: MetadataKey>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>,
    ) where
        Metadata<T>: View,
    {
        Self::register_with_renderer::<Metadata<T>>(dispatcher, |renderer, metadata, env| {
            renderer.render_any(metadata.content, env)
        });
    }

    /// Registers a handler that receives the dispatching [`WinUiRenderer`]
    /// directly, so handlers can recurse without touching the raw context
    /// pointer themselves.
    pub(crate) fn register_with_renderer<V: View>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>,
        handler: impl 'static + Clone + Fn(&mut Self, V, &Environment) -> UIElement,
    ) {
        dispatcher.register::<V>(move |_state, ctx, view, env| {
            // SAFETY: every `RenderContext` is created by
            // `WinUiRenderer::render`/`render_any` from a live `&mut
            // WinUiRenderer` immediately before the synchronous `dispatch`
            // call that invokes this handler, and rendering runs exclusively
            // on the UI thread.
            let renderer = unsafe { ctx.renderer() };
            handler(renderer, view, env)
        });
    }

    /// Registers a `Native<T>` wrapped component with the dispatcher.
    fn register_native<T: waterui_core::NativeView + 'static>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>,
    ) where
        Native<T>: WinUiComponent,
    {
        Self::register::<Native<T>>(dispatcher);
    }

    /// Registers a `WinUiComponent` view type with the dispatcher.
    fn register<V: WinUiComponent + View>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>,
    ) {
        Self::register_with_renderer::<V>(dispatcher, |renderer, view, env| {
            view.render(env, renderer)
        });
    }
}
