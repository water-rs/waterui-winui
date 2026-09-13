//! `WinUI` hosts for `WaterUI` layout objects.

use std::cell::RefCell;
use std::rc::Rc;

use waterui_core::layout::Layout;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::{LayoutPanel, WinUiSubView};

/// A shared handle to a [`LayoutPanel`]'s layout state.
///
/// `Panel::compose` consumes the implementation object, so the state lives
/// behind `Rc<RefCell>`: the renderer keeps this handle to install the
/// `WaterUI` layout after composition, and the composed panel reads it inside
/// `MeasureOverride`/`ArrangeOverride` on the UI thread.
pub(crate) type LayoutState = Rc<RefCell<Option<LayoutPanelState>>>;

pub(crate) struct LayoutPanelState {
    pub layout: Box<dyn Layout>,
    pub subviews: Vec<WinUiSubView>,
}

/// Creates a composed `Panel` whose measure/arrange runs `layout`.
///
/// The panel's `Children` are the subview elements in order.
pub(crate) fn layout_panel(
    layout: Box<dyn Layout>,
    subviews: Vec<WinUiSubView>,
) -> windows_core::Result<Panel> {
    let state: LayoutState = Rc::new(RefCell::new(None));
    let panel = Panel::compose(LayoutPanel::new(Rc::clone(&state)))?;
    {
        let children = crate::util::children(&panel);
        for subview in &subviews {
            children.Append(subview.element())?;
        }
    }
    *state.borrow_mut() = Some(LayoutPanelState { layout, subviews });
    Ok(panel)
}

/// An invisible, zero-size container used for views that render to nothing
/// (e.g. `()`).
pub(crate) fn empty_panel() -> windows_core::Result<Grid> {
    Grid::new()
}
