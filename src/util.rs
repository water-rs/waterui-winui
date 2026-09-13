//! Shared helpers for the `WinUI` backend.

#![allow(clippy::inline_always, clippy::ref_as_ptr)] // generated `implement` macro items
use std::cell::RefCell;

use nami::Signal;
use nami::watcher::{BoxWatcherGuard, Context};
use waterui_core::views::Views;
use waterui_core::{AnyView, Environment, IgnorableMetadata, Metadata};
use waterui_graphics::color::ResolvedColor;
use waterui_layout::StretchAxis;
use windows_core::{AsImpl, IInspectable, Interface, implement};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

/// The children of any `Panel` subclass as a mutable vector view.
///
/// The generated `UIElementCollection` derefs to this crate's methodless
/// `IVector` projection; the same IID is implemented by
/// `windows_collections::IVector`, so a cast recovers the full vector API.
/// `panel` may be any element that is a `Panel` (`Grid`, `StackPanel`, ...).
pub fn children<P: Interface>(panel: &P) -> windows_collections::IVector<UIElement> {
    panel
        .cast::<Panel>()
        .expect("element is a Panel")
        .Children()
        .and_then(|collection| collection.cast())
        .expect("Panel::Children")
}

/// Casts a generated collection interface to the full `IVector` projection.
pub fn vector<C: Interface, T: windows_core::RuntimeType + 'static>(
    collection: &C,
) -> windows_collections::IVector<T> {
    collection
        .cast::<windows_collections::IVector<T>>()
        .expect("collection is an IVector")
}

/// A `TextBlock`/`Span` `InlineCollection` as a mutable vector view.
pub fn inlines<C: Interface>(collection: &C) -> windows_collections::IVector<Inline> {
    vector(collection)
}

/// Installs a signal subscription before taking its initial snapshot.
///
/// The caller must keep the returned guard alive for as long as the native
/// element should keep updating — see [`store_watcher_guard`].
pub fn subscribe_then_get<S>(
    signal: &S,
    watcher: impl Fn(Context<S::Output>) + 'static,
) -> (S::Output, S::Guard)
where
    S: Signal,
{
    let guard = signal.watch(watcher);
    (signal.get(), guard)
}

/// Private COM marker interface used to round-trip [`ElementAttachments`]
/// through `FrameworkElement.Tag`. A `#[implement]` object without a declared
/// interface can only convert into `IInspectable`, never back; declaring one
/// private interface gives `QueryInterface` + `AsImpl` a channel. Only this
/// module ever queries the IID, so the empty vtable is never invoked.
#[windows_core::interface("d72da516-c77b-4bef-b176-b19068840254")]
unsafe trait IElementAttachments: windows_core::IUnknown {}

/// The per-element bag of things that must die with the element: nami watcher
/// guards and `WinRT` event revokers.
///
/// Stored as the element's `FrameworkElement.Tag`; XAML keeps the bag alive
/// exactly as long as the element.
#[implement(IElementAttachments)]
struct ElementAttachments {
    guards: RefCell<Vec<BoxWatcherGuard>>,
    revokers: RefCell<Vec<windows_core::EventRevoker>>,
    retained: RefCell<Vec<Box<dyn core::any::Any>>>,
}

impl IElementAttachments_Impl for ElementAttachments_Impl {}

impl ElementAttachments {
    fn new() -> Self {
        Self {
            guards: RefCell::new(Vec::new()),
            revokers: RefCell::new(Vec::new()),
            retained: RefCell::new(Vec::new()),
        }
    }
}

fn attachments(element: &FrameworkElement) -> windows_core::Result<IElementAttachments> {
    if let Ok(tag) = element.Tag()
        && let Ok(existing) = tag.cast::<IElementAttachments>()
    {
        return Ok(existing);
    }
    let fresh: IElementAttachments = ElementAttachments::new().into();
    element.SetTag(&fresh.cast::<IInspectable>()?)?;
    Ok(fresh)
}

/// Keeps `guard` alive for the lifetime of `element`.
pub fn store_watcher_guard(element: &FrameworkElement, guard: BoxWatcherGuard) {
    store_watcher_guards(element, std::iter::once(guard));
}

/// Keeps every guard alive for the lifetime of `element`.
pub fn store_watcher_guards(
    element: &FrameworkElement,
    guards: impl IntoIterator<Item = BoxWatcherGuard>,
) {
    let attachments = attachments(element)
        .expect("FrameworkElement.Tag must accept the WaterUI attachment object");
    // SAFETY: the interface was obtained from a tag this module set, so it is
    // backed by `ElementAttachments`.
    let bag: &ElementAttachments = unsafe { attachments.as_impl() };
    bag.guards.borrow_mut().extend(guards);
}

/// Keeps `revoker` alive for the lifetime of `element`; dropping it
/// unsubscribes the `WinRT` event.
pub fn store_event_revoker(element: &FrameworkElement, revoker: windows_core::EventRevoker) {
    let attachments = attachments(element)
        .expect("FrameworkElement.Tag must accept the WaterUI attachment object");
    // SAFETY: same as above — only `ElementAttachments` answers this IID.
    let bag: &ElementAttachments = unsafe { attachments.as_impl() };
    bag.revokers.borrow_mut().push(revoker);
}

/// Keeps an arbitrary value alive for the lifetime of `element`
/// (`Metadata<Retain>`).
pub fn store_retained(element: &FrameworkElement, value: Box<dyn core::any::Any>) {
    let attachments = attachments(element)
        .expect("FrameworkElement.Tag must accept the WaterUI attachment object");
    // SAFETY: same as above — only `ElementAttachments` answers this IID.
    let bag: &ElementAttachments = unsafe { attachments.as_impl() };
    bag.retained.borrow_mut().push(value);
}

/// Casts a `UIElement` to `FrameworkElement`.
///
/// Every element this backend produces is a `FrameworkElement` (all XAML
/// controls are); panics if that invariant is ever violated.
pub fn framework(element: &UIElement) -> FrameworkElement {
    element
        .cast::<FrameworkElement>()
        .expect("WaterUI WinUI elements are always FrameworkElements")
}

/// Converts a resolved `WaterUI` color to a `WinUI` `Color`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "channels are clamped to the target range before the cast"
)]
pub fn resolved_color_to_winui(color: &ResolvedColor) -> Color {
    let srgb = color.to_srgb_with_headroom();
    Color {
        a: (color.opacity.clamp(0.0, 1.0) * 255.0) as u8,
        r: (srgb.red.clamp(0.0, 1.0) * 255.0) as u8,
        g: (srgb.green.clamp(0.0, 1.0) * 255.0) as u8,
        b: (srgb.blue.clamp(0.0, 1.0) * 255.0) as u8,
    }
}

/// Builds a `SolidColorBrush` from a resolved color.
pub fn solid_brush(color: &ResolvedColor) -> windows_core::Result<SolidColorBrush> {
    let brush = SolidColorBrush::new()?;
    brush.SetColor(resolved_color_to_winui(color))?;
    Ok(brush)
}

/// Metadata wrappers that do not change a child's layout behavior.
///
/// Mirrors the GTK backend: stretching answers come from the semantic child,
/// not from the metadata shell.
fn passthrough_content(view: &AnyView) -> Option<&AnyView> {
    use waterui::accessibility::{
        AccessibilityChildren, AccessibilityHidden, AccessibilityLabel, AccessibilityRole,
        AccessibilityState, AccessibilityStateSignal,
    };
    use waterui::background::{Background, MaterialBackground};
    use waterui::border::Border;
    use waterui::component::focus::Focused;
    use waterui::cursor::Cursor;
    use waterui::drag_drop::{Draggable, DropDestination};
    use waterui::filter::Opacity;
    use waterui::gesture::GestureObserver;
    use waterui::interaction::Hittable;
    use waterui::metadata::context_menu::ResolvedContextMenu;
    use waterui::metadata::secure::{HighDynamicRange, Secure, StandardDynamicRange};
    use waterui::navigation::{NavigationTransitionDestination, NavigationTransitionSource};
    use waterui::style::{Offset, Rotation, Scale, Shadow};
    use waterui_core::Retain;
    use waterui_core::event::{LifeCycleHook, OnEvent};
    use waterui_layout::safe_area::IgnoreSafeArea;
    use waterui_shape::ClipShape;

    macro_rules! passthrough_metadata_content {
        ($($ty:ty),* $(,)?) => {
            $(
                if let Some(metadata) = view.downcast_ref::<Metadata<$ty>>() {
                    return Some(&metadata.content);
                }
            )*
        };
    }
    macro_rules! passthrough_ignorable_metadata_content {
        ($($ty:ty),* $(,)?) => {
            $(
                if let Some(metadata) = view.downcast_ref::<IgnorableMetadata<$ty>>() {
                    return Some(&metadata.content);
                }
            )*
        };
    }

    passthrough_metadata_content!(
        Environment,
        Retain,
        Opacity,
        Scale,
        Rotation,
        Offset,
        ClipShape,
        Border,
        Shadow,
        Focused,
        Hittable,
        GestureObserver,
        LifeCycleHook,
        OnEvent,
        Secure,
        StandardDynamicRange,
        HighDynamicRange,
        Cursor,
        IgnoreSafeArea,
        ResolvedContextMenu,
        Draggable,
        DropDestination,
        Background,
        NavigationTransitionSource,
        NavigationTransitionDestination
    );
    #[cfg(feature = "gpu")]
    passthrough_metadata_content!(waterui_graphics::AppliedFilter);
    passthrough_ignorable_metadata_content!(
        MaterialBackground,
        AccessibilityLabel,
        AccessibilityRole,
        AccessibilityHidden,
        AccessibilityChildren,
        AccessibilityState,
        AccessibilityStateSignal
    );

    None
}

/// Returns the stretch axis for layout, recursively unwrapping metadata
/// wrappers.
#[must_use]
pub fn effective_stretch_axis(view: &AnyView) -> StretchAxis {
    if let Some(content) = passthrough_content(view) {
        return effective_stretch_axis(content);
    }
    view.stretch_axis()
}

/// Renders each child view into a [`WinUiSubView`](crate::component::WinUiSubView),
/// taking the stretch axis from the view before it is consumed.
pub fn render_subviews(
    views: Vec<AnyView>,
    env: &Environment,
    renderer: &mut crate::renderer::WinUiRenderer,
) -> Vec<crate::component::WinUiSubView> {
    views
        .into_iter()
        .map(|view| {
            let axis = effective_stretch_axis(&view);
            let element = renderer.render_any(view, env);
            crate::component::WinUiSubView::new(element, axis)
        })
        .collect()
}

/// Materializes every view of a [`Views`] collection eagerly.
///
/// Used by containers that need all children up-front (`FixedContainer`).
/// Lazy collections get their own rendering path.
pub fn materialize_views<V: Views>(views: &V) -> Vec<V::View> {
    (0..views.len().get())
        .filter_map(|index| views.get_view(index))
        .collect()
}
