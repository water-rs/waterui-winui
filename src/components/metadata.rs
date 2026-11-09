//! Handlers for `Metadata<T>` and `IgnorableMetadata<T>` wrappers.
//!
//! Every registration mirrors the GTK backend's contract: render the wrapped
//! content first, then apply the metadata to the produced element. Watcher
//! guards and event revokers are pinned to the element through
//! `FrameworkElement.Tag` (see [`crate::util`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use executor_core::LocalExecutor;
use nami::Signal;
use waterui::Str;
use waterui::Url;
use waterui::accessibility::{
    AccessibilityChildren, AccessibilityHidden, AccessibilityIdentifier, AccessibilityLabel,
    AccessibilityRole, AccessibilityState, AccessibilityStateSignal,
};
use waterui::background::{Background, Material, MaterialBackground};
use waterui::border::Border;
use waterui::component::focus::Focused;
use waterui::cursor::{Cursor, CursorStyle};
use waterui::drag_drop::{
    DragPayload, Draggable, DropDestination, Files, PlatformRepresentation, TransferKind,
};
use waterui::filter::Opacity;
use waterui::gesture::{
    Gesture, GestureObserver, GesturePhase, GesturePoint, MagnificationEvent, RotationEvent,
    TapGesture,
};
use waterui::interaction::Hittable;
use waterui::metadata::context_menu::ResolvedContextMenu;
use waterui::metadata::secure::{HighDynamicRange, Secure, StandardDynamicRange};
use waterui::navigation::{NavigationTransitionDestination, NavigationTransitionSource};
use waterui::style::{Anchor, Offset, Rotation, Scale, Shadow};
use waterui_backend_core::ViewDispatcher;
use waterui_core::event::{Event, HoverEvent, LifeCycle, LifeCycleHook, OnEvent};
use waterui_core::handler::BoxedAction;
use waterui_core::key::{KeyHandling, KeyPress, OnKeyPress};
use waterui_core::layout::Point as LayoutPoint;
use waterui_core::{Environment, IgnorableMetadata, Metadata, Retain};
use waterui_layout::safe_area::IgnoreSafeArea;
use waterui_shape::{ClipShape, PathCommand, ShapeKind};
use windows_core::Interface;
use windows_numerics::{Vector2, Vector3};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::executor::DispatcherQueueExecutor;
use crate::renderer::{RenderContext, WinUiRenderer};
use crate::util::{
    framework, solid_brush, store_event_revoker, store_retained, store_watcher_guard,
    subscribe_then_get, working_color_to_winui,
};

/// Registers every metadata handler the `WinUI` backend supports.
pub(crate) fn register(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    register_environment(dispatcher);
    register_retain(dispatcher);
    register_lifecycle(dispatcher);
    register_opacity(dispatcher);
    register_shadow(dispatcher);
    register_focused(dispatcher);
    register_cursor(dispatcher);
    register_border(dispatcher);
    register_scale(dispatcher);
    register_rotation(dispatcher);
    register_offset(dispatcher);
    register_clip_shape(dispatcher);
    register_hittable(dispatcher);
    register_on_event(dispatcher);
    register_on_key_press(dispatcher);
    register_gesture_observer(dispatcher);
    register_context_menu(dispatcher);
    register_drag_drop(dispatcher);
    register_background(dispatcher);
    register_accessibility(dispatcher);
    register_passthroughs(dispatcher);
}

/// `Metadata<Environment>` — the value replaces the environment for the subtree.
fn register_environment(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Environment>>(
        dispatcher,
        |renderer, metadata, _env| renderer.render_any(metadata.content, &metadata.value),
    );
}

/// `Metadata<Retain>` — keep the value alive for the element's lifetime.
fn register_retain(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Retain>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            store_retained(&framework(&element), Box::new(metadata.value));
            element
        },
    );
}

/// `Metadata<LifeCycleHook>` — `Loaded` fires `Appear`, `Unloaded` fires
/// `Disappear`.
fn register_lifecycle(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<LifeCycleHook>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let fe = framework(&element);
            match metadata.value.lifecycle() {
                LifeCycle::Appear => {
                    let hook = RefCell::new(Some(metadata.value));
                    let env = env.clone();
                    let revoker = fe
                        .Loaded(move |_sender, _args| {
                            if let Some(hook) = hook.borrow_mut().take() {
                                hook.handle(&env);
                            }
                        })
                        .expect("FrameworkElement::Loaded");
                    store_event_revoker(&fe, revoker);
                }
                LifeCycle::Disappear => {
                    let hook = RefCell::new(Some(metadata.value));
                    let env = env.clone();
                    let revoker = fe
                        .Unloaded(move |_sender, _args| {
                            if let Some(hook) = hook.borrow_mut().take() {
                                hook.handle(&env);
                            }
                        })
                        .expect("FrameworkElement::Unloaded");
                    store_event_revoker(&fe, revoker);
                }
                _ => panic!("unsupported LifeCycle variant on WinUI backend"),
            }
            element
        },
    );
}

/// `Metadata<Opacity>` — reactive `UIElement.Opacity`.
fn register_opacity(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Opacity>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let (initial, guard) = subscribe_then_get(&metadata.value.value, {
                let element = element.clone();
                move |ctx| {
                    element
                        .SetOpacity(f64::from(ctx.into_value()))
                        .expect("UIElement::SetOpacity");
                }
            });
            element
                .SetOpacity(f64::from(initial))
                .expect("UIElement::SetOpacity");
            store_watcher_guard(&framework(&element), guard);
            element
        },
    );
}

/// `Metadata<Shadow>` — a compositor `DropShadow` behind the element.
fn register_shadow(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Shadow>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let shadow = metadata.value;
            apply_drop_shadow(&element, &shadow, env);
            element
        },
    );
}

/// Builds a compositor `DropShadow` sized to the element via an expression
/// animation bound to `host.Size`.
fn apply_drop_shadow(element: &UIElement, shadow: &Shadow, env: &Environment) {
    let visual = ElementCompositionPreview::GetElementVisual(element).expect("GetElementVisual");
    let compositor = visual
        .cast::<ICompositionObject>()
        .expect("ICompositionObject")
        .Compositor()
        .expect("CompositionObject::Compositor");

    let drop = compositor
        .cast::<ICompositor2>()
        .expect("ICompositor2")
        .CreateDropShadow()
        .expect("Compositor::CreateDropShadow");
    drop.SetOffset(Vector3 {
        x: shadow.offset.x,
        y: shadow.offset.y,
        z: 0.0,
    })
    .expect("DropShadow::SetOffset");
    drop.SetBlurRadius(shadow.radius)
        .expect("DropShadow::SetBlurRadius");

    let sprite = compositor
        .CreateSpriteVisual()
        .expect("Compositor::CreateSpriteVisual");
    sprite
        .cast::<ISpriteVisual2>()
        .expect("ISpriteVisual2")
        .SetShadow(&drop)
        .expect("ISpriteVisual2::SetShadow");

    let size_expr = compositor
        .CreateExpressionAnimation()
        .expect("CreateExpressionAnimation");
    size_expr
        .SetExpression("host.Size")
        .expect("ExpressionAnimation::SetExpression");
    size_expr
        .cast::<ICompositionAnimation>()
        .expect("ICompositionAnimation")
        .SetReferenceParameter("host", &visual)
        .expect("SetReferenceParameter");
    sprite
        .cast::<ICompositionObject>()
        .expect("ICompositionObject")
        .StartAnimation("Size", &size_expr)
        .expect("ICompositionObject::StartAnimation");

    ElementCompositionPreview::SetElementChildVisual(element, &sprite)
        .expect("SetElementChildVisual");

    // Composition objects do not implement IWeakReferenceSource, so the
    // watcher holds the shadow strongly; the guard lives on the element and
    // the shadow must outlive it anyway — there is no cycle to break.
    let watched = drop.clone();
    let (initial, guard) = subscribe_then_get(&shadow.color.resolve(env), move |ctx| {
        let resolved = ctx.into_value();
        watched
            .SetColor(working_color_to_winui(&resolved))
            .expect("DropShadow::SetColor");
    });
    drop.SetColor(working_color_to_winui(&initial))
        .expect("DropShadow::SetColor");
    store_watcher_guard(&framework(element), guard);
}

/// `Metadata<Focused>` — bridge a `Binding<bool>` with element focus.
fn register_focused(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Focused>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let fe = framework(&element);
            let binding = metadata.value.0;

            {
                let binding = binding.clone();
                let revoker = element
                    .GotFocus(move |_sender, _args| {
                        binding.set(true);
                    })
                    .expect("UIElement::GotFocus");
                store_event_revoker(&fe, revoker);
            }
            {
                let binding = binding.clone();
                let revoker = element
                    .LostFocus(move |_sender, _args| {
                        binding.set(false);
                    })
                    .expect("UIElement::LostFocus");
                store_event_revoker(&fe, revoker);
            }

            let weak = element.downgrade().expect("weak UIElement");
            let (initial, guard) = subscribe_then_get(&binding, move |ctx| {
                let focused = ctx.into_value();
                if let Some(element) = weak.upgrade()
                    && focused
                {
                    element
                        .Focus(FocusState::Programmatic)
                        .expect("UIElement::Focus");
                }
            });
            if initial {
                // The element may not be in the tree yet; defer to Loaded.
                let weak = element.downgrade().expect("weak UIElement");
                let revoker = fe
                    .Loaded(move |_sender, _args| {
                        if let Some(element) = weak.upgrade() {
                            element
                                .Focus(FocusState::Programmatic)
                                .expect("UIElement::Focus");
                        }
                    })
                    .expect("FrameworkElement::Loaded");
                store_event_revoker(&fe, revoker);
            }
            store_watcher_guard(&fe, guard);
            element
        },
    );
}

/// `Metadata<Cursor>` — `ProtectedCursor` from the pointer style.
fn register_cursor(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Cursor>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let weak = element.downgrade().expect("weak UIElement");
            let (initial, guard) = subscribe_then_get(&metadata.value.style, move |ctx| {
                let style = ctx.into_value();
                if let Some(element) = weak.upgrade() {
                    let cursor = InputSystemCursor::Create(cursor_shape(style))
                        .expect("InputSystemCursor::Create");
                    element
                        .cast::<IUIElementProtected>()
                        .expect("IUIElementProtected")
                        .SetProtectedCursor(&cursor)
                        .expect("IUIElementProtected::SetProtectedCursor");
                }
            });
            let cursor = InputSystemCursor::Create(cursor_shape(initial))
                .expect("InputSystemCursor::Create");
            element
                .cast::<IUIElementProtected>()
                .expect("IUIElementProtected")
                .SetProtectedCursor(&cursor)
                .expect("IUIElementProtected::SetProtectedCursor");
            store_watcher_guard(&framework(&element), guard);
            element
        },
    );
}

fn cursor_shape(style: CursorStyle) -> InputSystemCursorShape {
    match style {
        CursorStyle::PointingHand | CursorStyle::OpenHand | CursorStyle::ClosedHand => {
            InputSystemCursorShape::Hand
        }
        CursorStyle::IBeam => InputSystemCursorShape::IBeam,
        CursorStyle::Crosshair => InputSystemCursorShape::Cross,
        CursorStyle::NotAllowed => InputSystemCursorShape::UniversalNo,
        CursorStyle::ResizeLeft | CursorStyle::ResizeRight | CursorStyle::ResizeLeftRight => {
            InputSystemCursorShape::SizeWestEast
        }
        CursorStyle::ResizeUp | CursorStyle::ResizeDown | CursorStyle::ResizeUpDown => {
            InputSystemCursorShape::SizeNorthSouth
        }
        CursorStyle::Move => InputSystemCursorShape::SizeAll,
        CursorStyle::Wait => InputSystemCursorShape::Wait,
        _ => InputSystemCursorShape::Arrow,
    }
}

/// `Metadata<Border>` — wrap content in a `Border` element.
fn register_border(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Border>>(
        dispatcher,
        |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let border = metadata.value;
            let element = crate::bindings::Border::new().expect("Border::new");
            element.SetChild(&content).expect("Border::SetChild");
            element
                .SetBorderThickness(edges_to_thickness(border.edges, border.width))
                .expect("Border::SetBorderThickness");
            let radius = f64::from(border.corner_radius);
            element
                .SetCornerRadius(CornerRadius {
                    top_left: radius,
                    top_right: radius,
                    bottom_right: radius,
                    bottom_left: radius,
                })
                .expect("Border::SetCornerRadius");

            let weak = element.downgrade().expect("weak Border");
            let (initial, guard) = subscribe_then_get(&border.color.resolve(env), move |ctx| {
                let resolved = ctx.into_value();
                if let Some(element) = weak.upgrade() {
                    element
                        .SetBorderBrush(&solid_brush(&resolved).expect("SolidColorBrush"))
                        .expect("Border::SetBorderBrush");
                }
            });
            element
                .SetBorderBrush(&solid_brush(&initial).expect("SolidColorBrush"))
                .expect("Border::SetBorderBrush");
            store_watcher_guard(&framework(&element.cast().expect("UIElement")), guard);
            element.cast().expect("Border is a UIElement")
        },
    );
}

fn edges_to_thickness(edges: waterui_layout::EdgeSet, width: f32) -> Thickness {
    let w = f64::from(width);
    Thickness {
        left: if edges.leading { w } else { 0.0 },
        top: if edges.top { w } else { 0.0 },
        right: if edges.trailing { w } else { 0.0 },
        bottom: if edges.bottom { w } else { 0.0 },
    }
}

/// Tracks `ActualSize` and writes `CenterPoint = anchor * size` so `Scale` and
/// `Rotation` pivot around the requested normalized anchor.
#[allow(clippy::cast_possible_truncation)] // Xaml geometry APIs take f32; WaterUI uses f64
fn track_center_point(element: &UIElement, anchor: Anchor) {
    let fe = framework(element);
    let apply = move |element: &UIElement| {
        let fe = framework(element);
        let w = fe.ActualWidth().expect("ActualWidth") as f32;
        let h = fe.ActualHeight().expect("ActualHeight") as f32;
        element
            .SetCenterPoint(Vector3 {
                x: anchor.x * w,
                y: anchor.y * h,
                z: 0.0,
            })
            .expect("UIElement::SetCenterPoint");
    };
    apply(element);
    let revoker = fe
        .SizeChanged(move |sender, _args| {
            if let Ok(sender) = sender.ok() {
                apply(&sender.cast::<UIElement>().expect("UIElement"));
            }
        })
        .expect("FrameworkElement::SizeChanged");
    store_event_revoker(&fe, revoker);
}

/// `Metadata<Scale>` — `UIElement.Scale` with anchor pivot.
fn register_scale(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Scale>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let scale = metadata.value;
            track_center_point(&element, scale.anchor);

            let current = Rc::new(RefCell::new((0.0_f32, 0.0_f32)));
            let apply = {
                let element = element.clone();
                let current = current.clone();
                move || {
                    let (x, y) = *current.borrow();
                    element
                        .SetScale(Vector3 { x, y, z: 1.0 })
                        .expect("UIElement::SetScale");
                }
            };
            let (initial_x, x_guard) = subscribe_then_get(&scale.x, {
                let current = current.clone();
                let apply = apply.clone();
                move |ctx| {
                    current.borrow_mut().0 = ctx.into_value();
                    apply();
                }
            });
            current.borrow_mut().0 = initial_x;
            let (initial_y, y_guard) = subscribe_then_get(&scale.y, {
                let current = current.clone();
                let apply = apply.clone();
                move |ctx| {
                    current.borrow_mut().1 = ctx.into_value();
                    apply();
                }
            });
            current.borrow_mut().1 = initial_y;
            apply();
            let fe = framework(&element);
            store_watcher_guard(&fe, x_guard);
            store_watcher_guard(&fe, y_guard);
            element
        },
    );
}

/// `Metadata<Rotation>` — `UIElement.Rotation` (degrees, matching `WaterUI`).
fn register_rotation(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Rotation>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let rotation = metadata.value;
            track_center_point(&element, rotation.anchor);
            let (initial, guard) = subscribe_then_get(&rotation.angle, {
                let element = element.clone();
                move |ctx| {
                    element
                        .SetRotation(ctx.into_value())
                        .expect("UIElement::SetRotation");
                }
            });
            element
                .SetRotation(initial)
                .expect("UIElement::SetRotation");
            store_watcher_guard(&framework(&element), guard);
            element
        },
    );
}

/// `Metadata<Offset>` — `UIElement.Translation`.
fn register_offset(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Offset>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let offset = metadata.value;
            let current = Rc::new(RefCell::new((0.0_f32, 0.0_f32)));
            let apply = {
                let element = element.clone();
                let current = current.clone();
                move || {
                    let (x, y) = *current.borrow();
                    element
                        .SetTranslation(Vector3 { x, y, z: 0.0 })
                        .expect("UIElement::SetTranslation");
                }
            };
            let (initial_x, x_guard) = subscribe_then_get(&offset.x, {
                let current = current.clone();
                let apply = apply.clone();
                move |ctx| {
                    current.borrow_mut().0 = ctx.into_value();
                    apply();
                }
            });
            current.borrow_mut().0 = initial_x;
            let (initial_y, y_guard) = subscribe_then_get(&offset.y, {
                let current = current.clone();
                let apply = apply.clone();
                move |ctx| {
                    current.borrow_mut().1 = ctx.into_value();
                    apply();
                }
            });
            current.borrow_mut().1 = initial_y;
            apply();
            let fe = framework(&element);
            store_watcher_guard(&fe, x_guard);
            store_watcher_guard(&fe, y_guard);
            element
        },
    );
}

/// `Metadata<ClipShape>` — rectangle/compositor clip, sized with the element.
fn register_clip_shape(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<ClipShape>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            install_clip(&element, &metadata.value);
            element
        },
    );
}

#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
// Xaml geometry APIs take f32; WaterUI uses f64
fn install_clip(element: &UIElement, shape: &ClipShape) {
    let fe = framework(element);
    match shape.kind() {
        ShapeKind::Rect => {
            let geometry = RectangleGeometry::new().expect("RectangleGeometry::new");
            element.SetClip(&geometry).expect("UIElement::SetClip");
            let update = {
                let geometry = geometry.clone();
                move |element: &UIElement| {
                    let fe = framework(element);
                    let w = fe.ActualWidth().expect("ActualWidth") as f32;
                    let h = fe.ActualHeight().expect("ActualHeight") as f32;
                    geometry
                        .SetRect(Rect {
                            x: 0.0,
                            y: 0.0,
                            width: w,
                            height: h,
                        })
                        .expect("RectangleGeometry::SetRect");
                }
            };
            update(element);
            let revoker = fe
                .SizeChanged(move |sender, _args| {
                    if let Ok(sender) = sender.ok() {
                        update(&sender.cast::<UIElement>().expect("UIElement"));
                    }
                })
                .expect("FrameworkElement::SizeChanged");
            store_event_revoker(&fe, revoker);
        }
        ShapeKind::RoundedRect { .. }
        | ShapeKind::UnevenRoundedRect { .. }
        | ShapeKind::FixedRoundedRect { .. }
        | ShapeKind::FixedUnevenRoundedRect { .. }
        | ShapeKind::Capsule
        | ShapeKind::Circle
        | ShapeKind::Ellipse => {
            // Rounded and elliptical clips live on the compositor visual;
            // `UIElement::Clip` only accepts an axis-aligned `RectangleGeometry`.
            let visual =
                ElementCompositionPreview::GetElementVisual(element).expect("GetElementVisual");
            let compositor = visual
                .cast::<ICompositionObject>()
                .expect("ICompositionObject")
                .Compositor()
                .expect("CompositionObject::Compositor");
            let clip = compositor
                .cast::<ICompositor6>()
                .expect("ICompositor6")
                .CreateGeometricClip()
                .expect("CreateGeometricClip");
            visual
                .SetClip(&clip.cast::<CompositionClip>().expect("CompositionClip"))
                .expect("Visual::SetClip");

            let kind = shape.kind();
            let rounded = compositor
                .cast::<ICompositor5>()
                .expect("ICompositor5")
                .CreateRoundedRectangleGeometry()
                .expect("CreateRoundedRectangleGeometry");
            let ellipse = compositor
                .cast::<ICompositor5>()
                .expect("ICompositor5")
                .CreateEllipseGeometry()
                .expect("CreateEllipseGeometry");
            let update = move |element: &UIElement| {
                let fe = framework(element);
                let w = fe.ActualWidth().expect("ActualWidth") as f32;
                let h = fe.ActualHeight().expect("ActualHeight") as f32;
                match kind {
                    ShapeKind::Circle | ShapeKind::Ellipse => {
                        clip.SetGeometry(
                            &ellipse
                                .cast::<CompositionGeometry>()
                                .expect("CompositionGeometry"),
                        )
                        .expect("GeometricClip::SetGeometry");
                        ellipse
                            .SetCenter(Vector2 {
                                x: w / 2.0,
                                y: h / 2.0,
                            })
                            .expect("EllipseGeometry::SetCenter");
                        ellipse
                            .SetRadius(Vector2 {
                                x: w / 2.0,
                                y: h / 2.0,
                            })
                            .expect("EllipseGeometry::SetRadius");
                    }
                    _ => {
                        clip.SetGeometry(
                            &rounded
                                .cast::<CompositionGeometry>()
                                .expect("CompositionGeometry"),
                        )
                        .expect("GeometricClip::SetGeometry");
                        // `CompositionRoundedRectangleGeometry` carries one
                        // uniform radius; use the largest corner so the clip
                        // still contains the shape.
                        let radius = match kind {
                            ShapeKind::RoundedRect { corner_radius } => corner_radius * w.min(h),
                            ShapeKind::UnevenRoundedRect {
                                top_left,
                                top_right,
                                bottom_left,
                                bottom_right,
                            } => {
                                top_left.max(top_right).max(bottom_left).max(bottom_right)
                                    * w.min(h)
                            }
                            // Fixed radii are absolute lengths; the shape
                            // contract clamps them to half the shorter side.
                            ShapeKind::FixedRoundedRect { corner_radius } => {
                                corner_radius.min(w.min(h) / 2.0)
                            }
                            ShapeKind::FixedUnevenRoundedRect {
                                top_left,
                                top_right,
                                bottom_left,
                                bottom_right,
                            } => top_left
                                .max(top_right)
                                .max(bottom_left)
                                .max(bottom_right)
                                .min(w.min(h) / 2.0),
                            ShapeKind::Capsule => w.min(h) / 2.0,
                            _ => 0.0,
                        };
                        rounded
                            .SetSize(Vector2 { x: w, y: h })
                            .expect("RoundedRectangleGeometry::SetSize");
                        rounded
                            .SetOffset(Vector2 { x: 0.0, y: 0.0 })
                            .expect("RoundedRectangleGeometry::SetOffset");
                        rounded
                            .SetCornerRadius(Vector2 {
                                x: radius,
                                y: radius,
                            })
                            .expect("RoundedRectangleGeometry::SetCornerRadius");
                    }
                }
            };
            update(element);
            let revoker = fe
                .SizeChanged(move |sender, _args| {
                    if let Ok(sender) = sender.ok() {
                        update(&sender.cast::<UIElement>().expect("UIElement"));
                    }
                })
                .expect("FrameworkElement::SizeChanged");
            store_event_revoker(&fe, revoker);
        }
        ShapeKind::CustomPath => install_path_clip(element, shape.commands()),
    }
}

/// Clips `element` to a unit-space `WaterUI` path via the compositor.
///
/// The path is recorded into an `ID2D1PathGeometry` and reaches the compositor
/// as a `CompositionPath` (see [`crate::d2d`]); a 1×1 `CompositionViewBox`
/// stretched to `Fill` maps the unit coordinates onto the visual's bounds, so
/// the clip tracks resizes compositor-side with no `SizeChanged` handler.
fn install_path_clip(element: &UIElement, commands: &[PathCommand]) {
    let visual = ElementCompositionPreview::GetElementVisual(element).expect("GetElementVisual");
    let compositor = visual
        .cast::<ICompositionObject>()
        .expect("ICompositionObject")
        .Compositor()
        .expect("CompositionObject::Compositor");
    let clip = compositor
        .cast::<ICompositor6>()
        .expect("ICompositor6")
        .CreateGeometricClip()
        .expect("CreateGeometricClip");

    let path = crate::d2d::composition_path(commands).expect("CompositionPath");
    let geometry = compositor
        .cast::<ICompositor5>()
        .expect("ICompositor5")
        .CreatePathGeometryWithPath(&path)
        .expect("CreatePathGeometryWithPath");
    clip.SetGeometry(
        &geometry
            .cast::<CompositionGeometry>()
            .expect("CompositionGeometry"),
    )
    .expect("GeometricClip::SetGeometry");

    let view_box = compositor
        .cast::<ICompositor5>()
        .expect("ICompositor5")
        .CreateViewBox()
        .expect("CreateViewBox");
    view_box
        .SetSize(Vector2 { x: 1.0, y: 1.0 })
        .expect("ViewBox::SetSize");
    view_box
        .SetStretch(CompositionStretch::Fill)
        .expect("ViewBox::SetStretch");
    clip.SetViewBox(&view_box)
        .expect("GeometricClip::SetViewBox");

    visual
        .SetClip(&clip.cast::<CompositionClip>().expect("CompositionClip"))
        .expect("Visual::SetClip");
}

/// `Metadata<Hittable>` — `IsHitTestVisible` (+ `IsEnabled` for controls).
fn register_hittable(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Hittable>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let weak = element.downgrade().expect("weak UIElement");
            let apply = move |element: &UIElement, enabled: bool| {
                element
                    .SetIsHitTestVisible(enabled)
                    .expect("UIElement::SetIsHitTestVisible");
                if let Ok(control) = element.cast::<Control>() {
                    control
                        .SetIsEnabled(enabled)
                        .expect("Control::SetIsEnabled");
                }
            };
            let (initial, guard) = subscribe_then_get(&metadata.value.enabled, move |ctx| {
                let enabled = ctx.into_value();
                if let Some(element) = weak.upgrade() {
                    apply(&element, enabled);
                }
            });
            apply(&element, initial);
            store_watcher_guard(&framework(&element), guard);
            element
        },
    );
}

/// `Metadata<OnEvent>` — pointer enter/move/exit via `Pointer*` events.
fn register_on_event(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<OnEvent>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let fe = framework(&element);
            let handler = RefCell::new(metadata.value);
            let env = env.clone();
            let event = handler.borrow().event();
            let revoker = match event {
                Event::HoverEnter => element
                    .PointerEntered(move |_sender, _args| {
                        handler.borrow_mut().handle(&env);
                    })
                    .expect("UIElement::PointerEntered"),
                Event::HoverMove => element
                    .PointerMoved(move |_sender, args| {
                        let Ok(args) = args.ok() else { return };
                        let point = args
                            .GetCurrentPoint(None::<&UIElement>)
                            .expect("PointerRoutedEventArgs::GetCurrentPoint")
                            .Position()
                            .expect("PointerPoint::Position");
                        let hover_env =
                            env.extending(HoverEvent::new(LayoutPoint::new(point.x, point.y)));
                        handler.borrow_mut().handle(&hover_env);
                    })
                    .expect("UIElement::PointerMoved"),
                Event::HoverExit => element
                    .PointerExited(move |_sender, _args| {
                        handler.borrow_mut().handle(&env);
                    })
                    .expect("UIElement::PointerExited"),
                _ => panic!("unsupported OnEvent variant on WinUI backend"),
            };
            store_event_revoker(&fe, revoker);
            element
        },
    );
}

/// `Metadata<OnKeyPress>` — claim unconsumed keys as they bubble up.
fn register_on_key_press(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<OnKeyPress>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            // `KeyDown` is WinUI's bubble phase: it reaches this element only
            // after every focused descendant left the press unhandled, and it
            // climbs ancestors nearest-first until one marks it `Handled`.
            let handler = RefCell::new(metadata.value);
            let env = env.clone();
            let revoker = element
                .KeyDown(move |_sender, args| {
                    let Ok(args) = args.ok() else { return };
                    let Ok(key) = args.Key() else { return };
                    let Ok(status) = args.KeyStatus() else { return };
                    let press = KeyPress {
                        key: crate::key_input::surface_key(key, status.scan_code),
                        code: crate::key_input::surface_code(
                            status.scan_code,
                            status.is_extended_key,
                        ),
                        modifiers: crate::key_input::surface_modifiers(),
                        repeat: status.was_key_down,
                    };
                    if handler.borrow_mut().handle(&env.extending(press)) == KeyHandling::Handled {
                        args.SetHandled(true)
                            .expect("KeyRoutedEventArgs::SetHandled");
                    }
                })
                .expect("UIElement::KeyDown");
            store_event_revoker(&framework(&element), revoker);
            element
        },
    );
}

/// `Metadata<GestureObserver>` — map `WaterUI` gestures onto `WinUI` pointer and
/// manipulation events.
fn register_gesture_observer(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<GestureObserver>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            install_gesture(
                &element,
                &metadata.value.gesture,
                Rc::new(RefCell::new(metadata.value.action)),
                env.clone(),
            );
            element
        },
    );
}

#[allow(clippy::too_many_lines)] // flat registration/wiring code
fn install_gesture(
    element: &UIElement,
    gesture: &Gesture,
    action: Rc<RefCell<BoxedAction<()>>>,
    env: Environment,
) {
    let fe = framework(element);
    match gesture {
        Gesture::Tap(TapGesture { count, .. }) => match count {
            1 => {
                let revoker = element
                    .Tapped(move |_sender, _args| {
                        (action.borrow_mut())(&env);
                    })
                    .expect("UIElement::Tapped");
                store_event_revoker(&fe, revoker);
            }
            2 => {
                let revoker = element
                    .DoubleTapped(move |_sender, _args| {
                        (action.borrow_mut())(&env);
                    })
                    .expect("UIElement::DoubleTapped");
                store_event_revoker(&fe, revoker);
            }
            _ => panic!("TapGesture count > 2 unsupported on WinUI backend"),
        },
        Gesture::LongPress(_) => {
            let revoker = element
                .Holding(move |_sender, args| {
                    if let Ok(args) = args.ok()
                        && let Ok(HoldingState::Started) = args.HoldingState()
                    {
                        (action.borrow_mut())(&env);
                    }
                })
                .expect("UIElement::Holding");
            store_event_revoker(&fe, revoker);
        }
        Gesture::Drag(_) => {
            element
                .SetManipulationMode(
                    ManipulationModes::TranslateX
                        | ManipulationModes::TranslateY
                        | ManipulationModes::TranslateInertia,
                )
                .expect("UIElement::SetManipulationMode");
            let revoker = element
                .ManipulationDelta(move |_sender, _args| {
                    (action.borrow_mut())(&env);
                })
                .expect("UIElement::ManipulationDelta");
            store_event_revoker(&fe, revoker);
        }
        Gesture::Magnification(_) => {
            element
                .SetManipulationMode(ManipulationModes::Scale | ManipulationModes::ScaleInertia)
                .expect("UIElement::SetManipulationMode");
            let revoker = element
                .ManipulationDelta(move |_sender, args| {
                    let Ok(args) = args.ok() else { return };
                    let cumulative = args.Cumulative().expect("ManipulationDelta::Cumulative");
                    let delta = args.Delta().expect("ManipulationDelta::Delta");
                    let position = args.Position().expect("ManipulationDelta::Position");
                    let event_env = env.extending(MagnificationEvent {
                        phase: GesturePhase::Updated,
                        center: GesturePoint {
                            x: position.x,
                            y: position.y,
                        },
                        scale: cumulative.scale,
                        velocity: delta.scale,
                    });
                    (action.borrow_mut())(&event_env);
                })
                .expect("UIElement::ManipulationDelta");
            store_event_revoker(&fe, revoker);
        }
        Gesture::Rotation(_) => {
            element
                .SetManipulationMode(ManipulationModes::Rotate | ManipulationModes::RotateInertia)
                .expect("UIElement::SetManipulationMode");
            let revoker = element
                .ManipulationDelta(move |_sender, args| {
                    let Ok(args) = args.ok() else { return };
                    let cumulative = args.Cumulative().expect("ManipulationDelta::Cumulative");
                    let delta = args.Delta().expect("ManipulationDelta::Delta");
                    let position = args.Position().expect("ManipulationDelta::Position");
                    let event_env = env.extending(RotationEvent {
                        phase: GesturePhase::Updated,
                        center: GesturePoint {
                            x: position.x,
                            y: position.y,
                        },
                        angle: cumulative.rotation.to_radians(),
                        velocity: delta.rotation.to_radians(),
                    });
                    (action.borrow_mut())(&event_env);
                })
                .expect("UIElement::ManipulationDelta");
            store_event_revoker(&fe, revoker);
        }
        Gesture::Simultaneous(both) => {
            install_gesture(element, both.first(), action.clone(), env.clone());
            install_gesture(element, both.second(), action, env);
        }
        Gesture::Exclusive(exclusive) => {
            install_gesture(element, exclusive.first(), action.clone(), env.clone());
            install_gesture(element, exclusive.second(), action, env);
        }
        Gesture::Then(seq) => {
            install_gesture(element, seq.first(), action.clone(), env.clone());
            install_gesture(element, seq.then(), action, env);
        }
        _ => panic!("unsupported Gesture variant on WinUI backend"),
    }
}

/// `Metadata<ResolvedContextMenu>` — `ContextFlyout` built from menu items.
fn register_context_menu(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<ResolvedContextMenu>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let fe = framework(&element);
            let flyout = MenuFlyout::new().expect("MenuFlyout::new");
            let ui = renderer.ui_thread().clone();
            // A `.context_menu`'s chords are live only while the menu is
            // open (the Hydrolysis contract): the rows park their
            // accelerators, the flyout arms them on `Opened` and clears
            // them on `Closed`.
            let accelerators =
                Rc::new(crate::components::menus::ContextFlyoutAccelerators::default());
            let arming = crate::components::menus::ShortcutArming::WhileOpen(accelerators.clone());
            crate::components::menus::rebuild_flyout(
                &flyout,
                &metadata.value.items.snapshot(),
                env,
                &ui,
                &arming,
            );
            element
                .SetContextFlyout(&flyout)
                .expect("UIElement::SetContextFlyout");

            let flyout_base = flyout
                .cast::<FlyoutBase>()
                .expect("MenuFlyout is a FlyoutBase");
            let armed = accelerators.clone();
            let revoker = flyout_base
                .Opened(move |_sender, _args| armed.opened())
                .expect("FlyoutBase::Opened");
            store_event_revoker(&fe, revoker);
            let revoker = flyout_base
                .Closed(move |_sender, _args| accelerators.closed())
                .expect("FlyoutBase::Closed");
            store_event_revoker(&fe, revoker);

            let weak = flyout.downgrade().expect("weak MenuFlyout");
            let (_initial, guard) = subscribe_then_get(&metadata.value.items, {
                let env = env.clone();
                let ui = ui.clone();
                move |ctx| {
                    let items = ctx.into_value();
                    if let Some(flyout) = weak.upgrade() {
                        crate::components::menus::rebuild_flyout(
                            &flyout, &items, &env, &ui, &arming,
                        );
                    }
                }
            });
            store_watcher_guard(&fe, guard);
            element
        },
    );
}

/// Data package format id announcing a drag that carries an in-process
/// payload. The value itself never reaches the pasteboard; it travels through
/// [`IN_PROCESS_DRAG`] instead.
const IN_PROCESS_FORMAT: &str = "WaterUI.DragPayload.InProcess";

thread_local! {
    /// The payload of the in-process drag in flight on the UI thread.
    ///
    /// `WinUI` delivers every drag callback on the UI thread, so a thread-local
    /// stash needs no synchronization and can hold the `!Send` payload.
    static IN_PROCESS_DRAG: RefCell<Option<DragPayload>> = const { RefCell::new(None) };
}

/// The kind of payload the drag behind `view` carries, or `None` when the
/// package holds nothing a `WaterUI` destination can receive.
fn incoming_drag_kind(view: &DataPackageView) -> Option<TransferKind> {
    if view.Contains(IN_PROCESS_FORMAT).unwrap_or(false) {
        return IN_PROCESS_DRAG.with(|slot| slot.borrow().as_ref().map(DragPayload::kind));
    }
    for (format, kind) in [
        (StandardDataFormats::StorageItems(), TransferKind::Files),
        (StandardDataFormats::WebLink(), TransferKind::Url),
        (StandardDataFormats::Text(), TransferKind::Text),
    ] {
        if format.is_ok_and(|id| view.Contains(&id).unwrap_or(false)) {
            return Some(kind);
        }
    }
    None
}

/// Resolves each file `Url` in `files` to a `StorageFile` and writes the set
/// to the package, holding the drag open with the args' deferral.
///
/// `GetFileFromPathAsync` is awaited on the UI thread through `spawn_local`,
/// so every `!Send` `WinRT` object involved stays on the thread that owns it.
fn write_drag_files(
    package: &DataPackage,
    files: &Files,
    args: &DragStartingEventArgs,
    executor: &DispatcherQueueExecutor,
) {
    let paths: Vec<String> = files
        .urls()
        .iter()
        .filter_map(Url::to_file_path)
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let deferral = args
        .GetDeferral()
        .expect("DragStartingEventArgs::GetDeferral");
    let package = package.clone();
    executor.spawn_local(async move {
        let mut items = Vec::<IStorageItem>::new();
        for path in &paths {
            if let Ok(operation) = StorageFile::GetFileFromPathAsync(path)
                && let Ok(file) = operation.await
                && let Ok(item) = file.cast::<IStorageItem>()
            {
                items.push(item);
            }
        }
        if !items.is_empty() {
            // `IStorageItem`'s vector element type is `Option<IStorageItem>`
            // (nullable interfaces box their default).
            let iterable = windows_collections::IIterable::<IStorageItem>::from(
                items.into_iter().map(Some).collect::<Vec<_>>(),
            );
            package
                .SetStorageItems(&iterable, false)
                .expect("DataPackage::SetStorageItems");
        }
        deferral
            .Complete()
            .expect("DragOperationDeferral::Complete");
    });
}

/// Awaits `operation` on the UI thread, builds the payload, delivers it to
/// `destination`, then completes the drop deferral.
fn resolve_drop<T>(
    operation: windows_future::IAsyncOperation<T>,
    deferral: DragOperationDeferral,
    executor: &DispatcherQueueExecutor,
    destination: Rc<RefCell<DropDestination>>,
    env: Environment,
    build: impl FnOnce(&T) -> Option<DragPayload> + 'static,
) where
    T: windows_core::RuntimeType + 'static,
{
    executor.spawn_local(async move {
        if let Ok(value) = operation.await
            && let Some(payload) = build(&value)
        {
            destination.borrow_mut().deliver(payload, &env);
        }
        deferral
            .Complete()
            .expect("DragOperationDeferral::Complete");
    });
}

/// `Metadata<Draggable>` / `Metadata<DropDestination>` — `WinUI` drag & drop.
#[allow(clippy::too_many_lines)] // flat registration/wiring code
fn register_drag_drop(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<Metadata<Draggable>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let fe = framework(&element);
            element.SetCanDrag(true).expect("UIElement::SetCanDrag");
            let draggable = metadata.value;
            let executor = renderer.executor().clone();
            let revoker = element
                .DragStarting(move |_sender, args| {
                    let Ok(args) = args.ok() else { return };
                    let package = args.Data().expect("DragStartingEventArgs::Data");
                    let payload = draggable.payload();
                    match payload.platform_representation() {
                        PlatformRepresentation::Text(text) => package
                            .SetText(text.as_str())
                            .expect("DataPackage::SetText"),
                        PlatformRepresentation::Url(url) => {
                            let uri = Uri::CreateUri(url.as_str()).expect("Uri::CreateUri");
                            package
                                .cast::<IDataPackage2>()
                                .expect("IDataPackage2")
                                .SetWebLink(&uri)
                                .expect("IDataPackage2::SetWebLink");
                        }
                        PlatformRepresentation::Files(files) => {
                            write_drag_files(&package, files, args, &executor);
                        }
                        PlatformRepresentation::InProcess => {
                            let marker = PropertyValue::CreateString(IN_PROCESS_FORMAT)
                                .expect("PropertyValue::CreateString");
                            package
                                .SetData(IN_PROCESS_FORMAT, &marker)
                                .expect("DataPackage::SetData");
                            IN_PROCESS_DRAG.with(|slot| *slot.borrow_mut() = Some(payload));
                        }
                    }
                })
                .expect("UIElement::DragStarting");
            store_event_revoker(&fe, revoker);
            // The stash dies with the drag, not with the element.
            let revoker = element
                .DropCompleted(|_sender, _args| {
                    IN_PROCESS_DRAG.with(|slot| slot.borrow_mut().take());
                })
                .expect("UIElement::DropCompleted");
            store_event_revoker(&fe, revoker);
            element
        },
    );

    WinUiRenderer::register_with_renderer::<Metadata<DropDestination>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let fe = framework(&element);
            element.SetAllowDrop(true).expect("UIElement::SetAllowDrop");

            let destination = Rc::new(RefCell::new(metadata.value));
            let entered = Rc::new(Cell::new(false));
            let executor = renderer.executor().clone();

            {
                let env = env.clone();
                let destination = destination.clone();
                let entered = entered.clone();
                let revoker = element
                    .DragEnter(move |_sender, args| {
                        let Ok(args) = args.ok() else { return };
                        let view = args.DataView().expect("DragEventArgs::DataView");
                        if incoming_drag_kind(&view) != Some(destination.borrow().accepted_kind()) {
                            return;
                        }
                        args.SetAcceptedOperation(DataPackageOperation::Copy)
                            .expect("DragEventArgs::SetAcceptedOperation");
                        destination.borrow_mut().enter(&env);
                        entered.set(true);
                    })
                    .expect("UIElement::DragEnter");
                store_event_revoker(&fe, revoker);
            }
            {
                // `DragOver` fires continuously after `DragEnter`; re-asserting
                // the accepted operation keeps `Drop` eligible on controls
                // whose default handling would reset it to `None`.
                let destination = destination.clone();
                let revoker = element
                    .DragOver(move |_sender, args| {
                        let Ok(args) = args.ok() else { return };
                        let view = args.DataView().expect("DragEventArgs::DataView");
                        if incoming_drag_kind(&view) == Some(destination.borrow().accepted_kind()) {
                            args.SetAcceptedOperation(DataPackageOperation::Copy)
                                .expect("DragEventArgs::SetAcceptedOperation");
                        }
                    })
                    .expect("UIElement::DragOver");
                store_event_revoker(&fe, revoker);
            }
            {
                let env = env.clone();
                let destination = destination.clone();
                let entered = entered.clone();
                let revoker = element
                    .DragLeave(move |_sender, _args| {
                        if entered.replace(false) {
                            destination.borrow_mut().exit(&env);
                        }
                    })
                    .expect("UIElement::DragLeave");
                store_event_revoker(&fe, revoker);
            }
            {
                let env = env.clone();
                let destination = destination.clone();
                let entered = entered.clone();
                let revoker = element
                    .Drop(move |_sender, args| {
                        let Ok(args) = args.ok() else { return };
                        let view = args.DataView().expect("DragEventArgs::DataView");
                        let accepted = destination.borrow().accepted_kind();
                        if incoming_drag_kind(&view) != Some(accepted) {
                            return;
                        }
                        entered.set(false);
                        // The data view only lives for the event; resolve it
                        // under a deferral, then deliver on the UI thread.
                        let deferral = args.GetDeferral().expect("DragEventArgs::GetDeferral");
                        match accepted {
                            TransferKind::Text => {
                                let operation =
                                    view.GetTextAsync().expect("DataPackageView::GetTextAsync");
                                resolve_drop(
                                    operation,
                                    deferral,
                                    &executor,
                                    destination.clone(),
                                    env.clone(),
                                    |text| {
                                        Some(DragPayload::new(Str::from(text.to_string_lossy())))
                                    },
                                );
                            }
                            TransferKind::Url => {
                                let operation = view
                                    .cast::<IDataPackageView2>()
                                    .expect("IDataPackageView2")
                                    .GetWebLinkAsync()
                                    .expect("IDataPackageView2::GetWebLinkAsync");
                                resolve_drop(
                                    operation,
                                    deferral,
                                    &executor,
                                    destination.clone(),
                                    env.clone(),
                                    |uri| {
                                        uri.AbsoluteUri()
                                            .ok()
                                            .and_then(|link| link.parse::<Url>().ok())
                                            .map(DragPayload::new)
                                    },
                                );
                            }
                            TransferKind::Files => {
                                let operation = view
                                    .GetStorageItemsAsync()
                                    .expect("DataPackageView::GetStorageItemsAsync");
                                resolve_drop(
                                    operation,
                                    deferral,
                                    &executor,
                                    destination.clone(),
                                    env.clone(),
                                    |items| {
                                        let size = items.Size().expect("IVectorView::Size");
                                        let urls = (0..size)
                                            .filter_map(|index| {
                                                items.GetAt(index).and_then(|item| item.Path()).ok()
                                            })
                                            .map(Url::from_file_path)
                                            .collect::<Vec<_>>();
                                        Some(DragPayload::new(Files::new(urls)))
                                    },
                                );
                            }
                            TransferKind::InProcess(_) => {
                                deferral
                                    .Complete()
                                    .expect("DragOperationDeferral::Complete");
                                if let Some(payload) =
                                    IN_PROCESS_DRAG.with(|slot| slot.borrow().clone())
                                {
                                    destination.borrow_mut().deliver(payload, &env);
                                }
                            }
                        }
                    })
                    .expect("UIElement::Drop");
                store_event_revoker(&fe, revoker);
            }
            element
        },
    );
}

/// `Metadata<Background>` — passthrough; real backgrounds are composed by
/// `BackgroundView` (`FixedContainer` + a `Background` view).
fn register_background(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_passthrough_metadata::<Background>(dispatcher);
}

/// `IgnorableMetadata<MaterialBackground>` — an `AcrylicBrush` behind the
/// content, matching the platform material.
fn register_material_background(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<IgnorableMetadata<MaterialBackground>>(
        dispatcher,
        |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let border = crate::bindings::Border::new().expect("Border::new");
            border
                .SetBackground(&acrylic_brush(metadata.value.0))
                .expect("Border::SetBackground");
            border.SetChild(&content).expect("Border::SetChild");
            border.cast().expect("Border is a UIElement")
        },
    );
}

fn acrylic_brush(material: Material) -> AcrylicBrush {
    let brush = AcrylicBrush::new().expect("AcrylicBrush::new");
    let opacity = match material {
        Material::UltraThin => 0.5,
        Material::Thin => 0.65,
        Material::Regular => 0.8,
        Material::Thick => 0.9,
        Material::UltraThick => 0.95,
    };
    brush
        .SetTintOpacity(opacity)
        .expect("AcrylicBrush::SetTintOpacity");
    brush
}

/// Accessibility metadata — `AutomationProperties` statics.
fn register_accessibility(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityLabel>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let weak = element.downgrade().expect("weak UIElement");
            let (initial, guard) = subscribe_then_get(metadata.value.signal(), move |ctx| {
                let label = ctx.into_value();
                if let Some(element) = weak.upgrade() {
                    AutomationProperties::SetName(
                        &element
                            .cast::<DependencyObject>()
                            .expect("DependencyObject"),
                        label.as_str(),
                    )
                    .expect("AutomationProperties::SetName");
                }
            });
            AutomationProperties::SetName(
                &element
                    .cast::<DependencyObject>()
                    .expect("DependencyObject"),
                initial.as_str(),
            )
            .expect("AutomationProperties::SetName");
            store_watcher_guard(&framework(&element), guard);
            element
        },
    );

    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityIdentifier>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            AutomationProperties::SetAutomationId(
                &element
                    .cast::<DependencyObject>()
                    .expect("DependencyObject"),
                metadata.value.as_str().as_str(),
            )
            .expect("AutomationProperties::SetAutomationId");
            element
        },
    );

    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityRole>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            apply_accessibility_role(&element, &metadata.value);
            element
        },
    );

    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityHidden>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            if metadata.value.is_hidden() {
                AutomationProperties::SetAccessibilityView(
                    &element
                        .cast::<DependencyObject>()
                        .expect("DependencyObject"),
                    AccessibilityView::Raw,
                )
                .expect("AutomationProperties::SetAccessibilityView");
            }
            element
        },
    );

    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityChildren>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            if metadata.value.excludes_descendants() {
                hide_descendants(&element);
            }
            element
        },
    );

    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityState>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            apply_accessibility_state(&element, &metadata.value);
            element
        },
    );

    WinUiRenderer::register_with_renderer::<IgnorableMetadata<AccessibilityStateSignal>>(
        dispatcher,
        |renderer, metadata, env| {
            let element = renderer.render_any(metadata.content, env);
            let weak = element.downgrade().expect("weak UIElement");
            let (initial, guard) = subscribe_then_get(metadata.value.state(), move |ctx| {
                let state = ctx.into_value();
                if let Some(element) = weak.upgrade() {
                    apply_accessibility_state(&element, &state);
                }
            });
            apply_accessibility_state(&element, &initial);
            store_watcher_guard(&framework(&element), guard);
            element
        },
    );
}

fn apply_accessibility_role(element: &UIElement, role: &AccessibilityRole) {
    let dep = element
        .cast::<DependencyObject>()
        .expect("DependencyObject");
    let (landmark, control_type) = match role {
        AccessibilityRole::Navigation => (AutomationLandmarkType::Navigation, "navigation"),
        AccessibilityRole::Main => (AutomationLandmarkType::Main, "main"),
        AccessibilityRole::Search => (AutomationLandmarkType::Search, "search"),
        AccessibilityRole::Footer => (AutomationLandmarkType::Custom, "contentinfo"),
        AccessibilityRole::Header => (AutomationLandmarkType::None, "heading"),
        AccessibilityRole::Button => (AutomationLandmarkType::None, "button"),
        AccessibilityRole::Link => (AutomationLandmarkType::None, "link"),
        AccessibilityRole::Image => (AutomationLandmarkType::None, "image"),
        AccessibilityRole::Text => (AutomationLandmarkType::None, "text"),
        AccessibilityRole::Article => (AutomationLandmarkType::None, "article"),
        AccessibilityRole::Section => (AutomationLandmarkType::None, "section"),
        AccessibilityRole::List => (AutomationLandmarkType::None, "list"),
        AccessibilityRole::ListItem => (AutomationLandmarkType::None, "listitem"),
        AccessibilityRole::Checkbox => (AutomationLandmarkType::None, "checkbox"),
        AccessibilityRole::RadioButton => (AutomationLandmarkType::None, "radio"),
        AccessibilityRole::Switch => (AutomationLandmarkType::None, "switch"),
        AccessibilityRole::Slider => (AutomationLandmarkType::None, "slider"),
        AccessibilityRole::ProgressBar => (AutomationLandmarkType::None, "progressbar"),
        AccessibilityRole::Tab => (AutomationLandmarkType::None, "tab"),
        AccessibilityRole::TabList => (AutomationLandmarkType::None, "tablist"),
        AccessibilityRole::TabPanel => (AutomationLandmarkType::None, "tabpanel"),
        AccessibilityRole::Menu => (AutomationLandmarkType::None, "menu"),
        AccessibilityRole::MenuItem => (AutomationLandmarkType::None, "menuitem"),
        AccessibilityRole::MenuBar => (AutomationLandmarkType::None, "menubar"),
        AccessibilityRole::MenuItemCheckbox => (AutomationLandmarkType::None, "menuitemcheckbox"),
        AccessibilityRole::MenuItemRadio => (AutomationLandmarkType::None, "menuitemradio"),
        AccessibilityRole::Combobox => (AutomationLandmarkType::None, "combobox"),
        _ => (AutomationLandmarkType::None, "group"),
    };
    if landmark != AutomationLandmarkType::None {
        AutomationProperties::SetLandmarkType(&dep, landmark)
            .expect("AutomationProperties::SetLandmarkType");
    }
    AutomationProperties::SetLocalizedControlType(&dep, control_type)
        .expect("AutomationProperties::SetLocalizedControlType");
}

fn apply_accessibility_state(element: &UIElement, state: &AccessibilityState) {
    let dep = element
        .cast::<DependencyObject>()
        .expect("DependencyObject");
    if state.is_hidden() {
        AutomationProperties::SetAccessibilityView(&dep, AccessibilityView::Raw)
            .expect("AutomationProperties::SetAccessibilityView");
    }
    if let Ok(control) = element.cast::<Control>() {
        control
            .SetIsEnabled(!state.is_disabled())
            .expect("Control::SetIsEnabled");
    }
    if state.is_busy() {
        AutomationProperties::SetLiveSetting(&dep, AutomationLiveSetting::Assertive)
            .expect("AutomationProperties::SetLiveSetting");
    }
    if let Some(checked) = state.checked_state() {
        AutomationProperties::SetItemStatus(
            &dep,
            match checked {
                waterui::accessibility::AccessibilityChecked::True => "checked",
                waterui::accessibility::AccessibilityChecked::False => "unchecked",
                waterui::accessibility::AccessibilityChecked::Mixed => "mixed",
            },
        )
        .expect("AutomationProperties::SetItemStatus");
    }
    if let Some(expanded) = state.expanded_state() {
        AutomationProperties::SetItemStatus(&dep, if expanded { "expanded" } else { "collapsed" })
            .expect("AutomationProperties::SetItemStatus");
    }
}

/// Hides every descendant `UIElement` from accessibility views.
fn hide_descendants(element: &UIElement) {
    if let Ok(panel) = element.cast::<Panel>() {
        let children = crate::util::children(&panel);
        for index in 0..children.Size().expect("IVector::Size") {
            let child = children.GetAt(index).expect("IVector::GetAt");
            AutomationProperties::SetAccessibilityView(
                &child.cast::<DependencyObject>().expect("DependencyObject"),
                AccessibilityView::Raw,
            )
            .expect("AutomationProperties::SetAccessibilityView");
            hide_descendants(&child);
        }
    } else if let Ok(content) = element.cast::<ContentControl>()
        && let Ok(child) = content.Content()
    {
        let child: UIElement = child.cast().expect("ContentControl content is UIElement");
        AutomationProperties::SetAccessibilityView(
            &child.cast::<DependencyObject>().expect("DependencyObject"),
            AccessibilityView::Raw,
        )
        .expect("AutomationProperties::SetAccessibilityView");
        hide_descendants(&child);
    }
}

/// Metadata kinds with no `WinUI` semantic — passthrough.
fn register_passthroughs(dispatcher: &mut ViewDispatcher<(), RenderContext, UIElement>) {
    WinUiRenderer::register_passthrough_metadata::<Secure>(dispatcher);
    WinUiRenderer::register_passthrough_metadata::<StandardDynamicRange>(dispatcher);
    WinUiRenderer::register_passthrough_metadata::<HighDynamicRange>(dispatcher);
    WinUiRenderer::register_passthrough_metadata::<IgnoreSafeArea>(dispatcher);
    WinUiRenderer::register_passthrough_metadata::<NavigationTransitionSource>(dispatcher);
    WinUiRenderer::register_passthrough_metadata::<NavigationTransitionDestination>(dispatcher);
    register_material_background(dispatcher);
}
