//! The component abstraction every `WaterUI` view is rendered through.

#![allow(clippy::inline_always, clippy::ref_as_ptr)] // generated `implement` macro items
use waterui_core::Environment;
use waterui_core::layout::{
    ProposalSize, Rect as LayoutRect, StretchAxis, SubView, ViewDimensions, measure_layout,
};
use windows_core::implement;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::renderer::WinUiRenderer;

/// Implemented by every `WaterUI` view type that maps onto a `WinUI` element.
pub trait WinUiComponent {
    /// Renders the view into a native `WinUI` element.
    ///
    /// `renderer` is used for recursive rendering of child views.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement;
}

/// A `UIElement` paired with the layout information `WaterUI` needs to
/// measure and place it.
pub struct WinUiSubView {
    element: UIElement,
    stretch_axis: StretchAxis,
    priority: i32,
    /// Set while the owning panel runs its arrange pass. `Layout::place`
    /// may query child sizes again, but `UIElement::Measure` is illegal
    /// during arrange — the query then answers from `DesiredSize` instead.
    arranging: std::cell::Cell<bool>,
}

impl WinUiSubView {
    /// Wraps a rendered element with its declared stretch axis.
    pub fn new(element: UIElement, stretch_axis: StretchAxis) -> Self {
        Self {
            element,
            stretch_axis,
            priority: 0,
            arranging: std::cell::Cell::new(false),
        }
    }

    /// The wrapped element.
    pub fn element(&self) -> &UIElement {
        &self.element
    }

    /// Marks the subview as inside (or outside) the arrange pass.
    pub(crate) fn set_arranging(&self, arranging: bool) {
        self.arranging.set(arranging);
    }

    fn desired_dimensions(&self) -> ViewDimensions {
        let desired = self
            .element
            .DesiredSize()
            .expect("UIElement::DesiredSize failed after Measure");
        ViewDimensions::new(waterui_core::layout::Size::new(
            desired.width,
            desired.height,
        ))
    }
}

impl SubView for WinUiSubView {
    /// Measures the element against `proposal` via `UIElement::Measure`.
    ///
    /// `None` maps to an unbounded proposal; a concrete extent caps the
    /// available size on that axis. On an axis the child declares it
    /// stretches along, the offer is the answer — reporting `DesiredSize`
    /// would shrink-wrap the child at whatever its template happened to
    /// measure (a `TabView` desires only its strip height, collapsing its
    /// own `*` content row). During arrange the element's measured
    /// `DesiredSize` is read instead of calling `Measure`, which inside the
    /// arrange pass re-invalidates the parent and produces a XAML layout
    /// cycle.
    fn measure(&self, proposal: ProposalSize) -> ViewDimensions {
        if !self.arranging.get() {
            self.element
                .Measure(Size {
                    width: proposal.width.unwrap_or(f32::INFINITY),
                    height: proposal.height.unwrap_or(f32::INFINITY),
                })
                .expect("UIElement::Measure failed during layout");
        }
        let desired = self.desired_dimensions().size;
        let width = if self.stretch_axis.stretches_horizontal() {
            proposal.width.unwrap_or(desired.width)
        } else {
            desired.width
        };
        let height = if self.stretch_axis.stretches_vertical() {
            proposal.height.unwrap_or(desired.height)
        } else {
            desired.height
        };
        ViewDimensions::new(waterui_core::layout::Size::new(width, height))
    }

    fn stretch_axis(&self) -> StretchAxis {
        self.stretch_axis
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

/// A `Panel` subclass whose measure/arrange delegates to a `WaterUI`
/// [`Layout`]. Children are the `Panel`'s own `Children`, in the same order
/// as `subviews`.
///
/// `Panel::compose` consumes the implementation object, so the layout state
/// is shared through [`LayoutState`](crate::layout::LayoutState): the caller
/// keeps a handle to install the layout after composition.
#[implement(IFrameworkElementOverrides)]
pub(crate) struct LayoutPanel {
    state: crate::layout::LayoutState,
}

impl LayoutPanel {
    pub(crate) fn new(state: crate::layout::LayoutState) -> Self {
        Self { state }
    }
}

impl IFrameworkElementOverrides_Impl for LayoutPanel_Impl {
    fn MeasureOverride(&self, available_size: &Size) -> windows_core::Result<Size> {
        let state = self.state.borrow();
        let Some(state) = state.as_ref() else {
            return Ok(*available_size);
        };
        let children: Vec<&dyn SubView> = state
            .subviews
            .iter()
            .map(|sub| sub as &dyn SubView)
            .collect();
        let proposal = ProposalSize::new(
            finite_or_none(available_size.width),
            finite_or_none(available_size.height),
        );
        state.selected_proposal.set(Some(proposal));
        let measured = measure_layout(&*state.layout, proposal, &children);
        Ok(Size {
            width: measured.size.width,
            height: measured.size.height,
        })
    }

    fn ArrangeOverride(&self, final_size: &Size) -> windows_core::Result<Size> {
        let state = self.state.borrow();
        let Some(state) = state.as_ref() else {
            return Ok(*final_size);
        };
        let children: Vec<&dyn SubView> = state
            .subviews
            .iter()
            .map(|sub| sub as &dyn SubView)
            .collect();
        let bounds = LayoutRect::from_size(waterui_core::layout::Size::new(
            final_size.width,
            final_size.height,
        ));
        // `place` may query child sizes; arrange-phase queries must not run
        // `UIElement::Measure`, so subviews answer from `DesiredSize` instead.
        for subview in &state.subviews {
            subview.set_arranging(true);
        }
        // `place` reuses the measurement offer XAML delivered to
        // `MeasureOverride`; only a panel arranged without a measured state
        // reconstructs one from the arrange bounds.
        let proposal = state.selected_proposal.get().unwrap_or_else(|| {
            ProposalSize::new(
                finite_or_none(final_size.width),
                finite_or_none(final_size.height),
            )
        });
        let placements = state.layout.place(bounds, proposal, &children);
        for subview in &state.subviews {
            subview.set_arranging(false);
        }
        for (subview, placement) in state.subviews.iter().zip(placements.iter()) {
            subview.element().Arrange(Rect {
                x: placement.frame.x(),
                y: placement.frame.y(),
                width: placement.frame.width(),
                height: placement.frame.height(),
            })?;
        }
        Ok(*final_size)
    }

    fn OnApplyTemplate(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn GoToElementStateCore(
        &self,
        _state_name: &windows_core::HSTRING,
        _use_transitions: bool,
    ) -> windows_core::Result<bool> {
        Ok(false)
    }
}

const fn finite_or_none(value: f32) -> Option<f32> {
    if value.is_finite() { Some(value) } else { None }
}
