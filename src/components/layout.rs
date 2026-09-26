//! Layout containers: spacer, fixed/lazy containers, divider, scroll view.

use nami::Signal;
use waterui::prelude::Divider;
use waterui_core::layout::Point;
use waterui_core::{Environment, Native};
use waterui_layout::container::{FixedContainer, LazyContainer};
use waterui_layout::scroll::{Axis, ScrollView};
use waterui_layout::spacer::Spacer;
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::layout::layout_panel;
use crate::renderer::WinUiRenderer;
use crate::util::{framework, materialize_views, render_subviews, store_watcher_guard};

impl WinUiComponent for Native<Spacer> {
    /// An invisible element that claims space through `StretchAxis`.
    fn render(self, _env: &Environment, _renderer: &mut WinUiRenderer) -> UIElement {
        // A spacer draws nothing; its parent's layout gives it a frame.
        let element = crate::layout::empty_panel().expect("Grid::new");
        element.cast().expect("Grid is a UIElement")
    }
}

impl WinUiComponent for Native<FixedContainer> {
    /// Composes a `Panel` whose measure/arrange runs the `WaterUI` `Layout`.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let (layout, contents) = self.into_inner().into_inner();
        let subviews = render_subviews(contents, env, renderer);
        let panel = layout_panel(layout, subviews).expect("layout panel");
        panel.cast().expect("Panel is a UIElement")
    }
}

impl WinUiComponent for Native<LazyContainer> {
    /// Materializes the lazy collection eagerly into a layout panel.
    ///
    /// True view-materialization-on-demand needs `ItemsRepeater` with an
    /// `IElementFactory` backed by the `AnyViews` store; the composed
    /// `Panel` + `Layout` path is used first so that correctness is
    /// established before virtualization.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let (layout, contents) = self.into_inner().into_inner();
        let views: Vec<waterui_core::AnyView> = materialize_views(&contents);
        let subviews = render_subviews(views, env, renderer);
        let panel = layout_panel(layout, subviews).expect("layout panel");
        panel.cast().expect("Panel is a UIElement")
    }
}

impl WinUiComponent for Divider {
    /// A hairline separator — a `Rectangle` filled by the theme's divider
    /// brush would need theme resource lookup; a 1-px `Border` with the
    /// `DividerStrokeColorDefaultBrush` resource is the WinUI-native answer.
    fn render(self, _env: &Environment, _renderer: &mut WinUiRenderer) -> UIElement {
        let border = Border::new().expect("Border::new");
        framework(&border.cast().expect("Border is a UIElement"))
            .SetHeight(1.0)
            .expect("FrameworkElement::SetHeight");
        // `DividerStrokeColorDefaultBrush` is defined by the WinUI theme
        // resources; fall back to a neutral gray if lookup fails at runtime.
        let brush = theme_resource_brush("DividerStrokeColorDefaultBrush")
            .or_else(|| theme_resource_brush("TextFillColorTertiaryBrush"));
        if let Some(brush) = brush {
            border.SetBackground(&brush).expect("Border::SetBackground");
        }
        border.cast().expect("Border is a UIElement")
    }
}

/// Looks up a `Brush` theme resource on the current `Application`.
fn theme_resource_brush(key: &str) -> Option<Brush> {
    let app = Application::Current().ok()?;
    let resources = app.Resources().ok()?;
    let map = resources
        .cast::<windows_collections::IMap<windows_core::IInspectable, windows_core::IInspectable>>()
        .ok()?;
    // Resource keys are boxed strings; `PropertyValue::CreateString` boxes.
    let key = PropertyValue::CreateString(key).ok()?;
    if !map.HasKey(&key).ok()? {
        return None;
    }
    map.Lookup(&key).ok()?.cast::<Brush>().ok()
}

impl WinUiComponent for Native<ScrollView> {
    /// Renders `ScrollViewer` with axis-dependent scrollbar visibility.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let (axis, content, controller) = self.into_inner().into_inner();

        let viewer = ScrollViewer::new().expect("ScrollViewer::new");
        let (h, v) = match axis {
            Axis::Horizontal => (ScrollBarVisibility::Auto, ScrollBarVisibility::Disabled),
            Axis::Vertical => (ScrollBarVisibility::Disabled, ScrollBarVisibility::Auto),
            Axis::All => (ScrollBarVisibility::Auto, ScrollBarVisibility::Auto),
            _ => panic!("unsupported scroll axis on the WinUI backend: {axis:?}"),
        };
        viewer
            .SetHorizontalScrollBarVisibility(h)
            .expect("ScrollViewer::SetHorizontalScrollBarVisibility");
        viewer
            .SetVerticalScrollBarVisibility(v)
            .expect("ScrollViewer::SetVerticalScrollBarVisibility");

        let content = renderer.render_any(content, env);
        viewer
            .cast::<ContentControl>()
            .expect("ScrollViewer is a ContentControl")
            .SetContent(&content)
            .expect("ContentControl::SetContent");

        if let Some(controller) = controller {
            let queue = renderer.executor().queue().clone();
            let weak = viewer.downgrade().expect("ScrollViewer supports weak refs");
            let generation = controller.generation();
            let target = controller.target();
            let guard = generation.watch(move |_| {
                let target = target.snapshot();
                let weak = weak.clone();
                let queue = queue.clone();
                enqueue_on_ui_thread(&queue, move || {
                    if let Some(viewer) = weak.upgrade() {
                        apply_scroll_target(&viewer, axis, target);
                    }
                });
            });
            let element: FrameworkElement =
                viewer.cast().expect("ScrollViewer is a FrameworkElement");
            store_watcher_guard(&element, guard);
            return element.cast().expect("FrameworkElement is a UIElement");
        }

        viewer.cast().expect("ScrollViewer is a UIElement")
    }
}

fn apply_scroll_target(viewer: &ScrollViewer, axis: Axis, target: Point) {
    let horizontal = match axis {
        Axis::Horizontal | Axis::All => Some(f64::from(target.x)),
        Axis::Vertical => None,
        _ => panic!("unsupported scroll axis on the WinUI backend: {axis:?}"),
    };
    let vertical = match axis {
        Axis::Vertical | Axis::All => Some(f64::from(target.y)),
        Axis::Horizontal => None,
        _ => panic!("unsupported scroll axis on the WinUI backend: {axis:?}"),
    };
    let _ = viewer.ChangeView(horizontal, vertical, None);
}
