//! Direct2D path geometry bridged into Windows composition.
//!
//! `CompositionGeometricClip` resolves a `CompositionPath` through
//! `IGeometrySource2DInterop` — the same contract `Win2D`'s `CanvasGeometry`
//! implements. Rather than taking a `Win2D` dependency, the backend implements
//! that interop directly over in-box Direct2D: `PathCommand`s are recorded
//! into an `ID2D1PathGeometry`, wrapped in a COM object exposing
//! `IGeometrySource2D` + `IGeometrySource2DInterop`, and handed to
//! `CompositionPath::Create`.

use std::f32::consts::PI;

use waterui_shape::PathCommand;
use windows_core::{Interface, imp::Type, implement};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

/// Builds a `CompositionPath` from unit-space `WaterUI` path commands.
///
/// The returned path keeps the 0.0–1.0 coordinates; pairing it with a
/// `CompositionViewBox` sized 1×1 and stretched to `Fill` lets the compositor
/// map it onto the clipped visual and keep tracking layout changes.
pub(crate) fn composition_path(commands: &[PathCommand]) -> windows_core::Result<CompositionPath> {
    let source: IGeometrySource2D = PathGeometrySource {
        geometry: path_geometry(commands)?,
    }
    .into();
    CompositionPath::Create(&source)
}

/// COM object handing a recorded `ID2D1PathGeometry` to the compositor.
///
/// `IGeometrySource2D` is the marker interface `CompositionPath::Create`
/// requires; `IGeometrySource2DInterop` is what the compositor actually
/// queries to extract the geometry.
#[implement(IGeometrySource2D, IGeometrySource2DInterop)]
struct PathGeometrySource {
    geometry: ID2D1PathGeometry,
}

impl IGeometrySource2D_Impl for PathGeometrySource_Impl {}

impl IGeometrySource2DInterop_Impl for PathGeometrySource_Impl {
    fn GetGeometry(&self, value: *mut *mut core::ffi::c_void) -> windows_core::Result<()> {
        // `ID2D1PathGeometry` derives from `ID2D1Geometry`, so the object
        // pointer is itself a valid `ID2D1Geometry*`; the clone transfers one
        // reference to the caller.
        unsafe { *value = Interface::into_raw(self.geometry.clone()) };
        Ok(())
    }

    fn TryGetGeometryUsingFactory(
        &self,
        _factory: *mut core::ffi::c_void,
        value: *mut *mut core::ffi::c_void,
    ) -> windows_core::Result<()> {
        // A recorded path geometry is factory-agnostic; hand back the same
        // object regardless of the caller's factory.
        self.GetGeometry(value)
    }
}

fn point(x: f32, y: f32) -> D2D1_POINT_2F {
    D2D1_POINT_2F { x, y }
}

/// Records `commands` into a new `ID2D1PathGeometry`.
fn path_geometry(commands: &[PathCommand]) -> windows_core::Result<ID2D1PathGeometry> {
    unsafe {
        let mut raw = core::ptr::null_mut();
        let iid = ID2D1Factory::IID;
        D2D1CreateFactory(
            D2D1_FACTORY_TYPE_SINGLE_THREADED,
            &raw const iid,
            core::ptr::null(),
            &raw mut raw,
        )
        .ok()?;
        let factory: ID2D1Factory = Type::from_abi(raw)?;

        let mut raw = core::ptr::null_mut();
        factory.CreatePathGeometry(&raw mut raw).ok()?;
        let geometry: ID2D1PathGeometry = Type::from_abi(raw)?;

        let mut raw = core::ptr::null_mut();
        geometry.Open(&raw mut raw).ok()?;
        let sink: ID2D1GeometrySink = Type::from_abi(raw)?;

        record_commands(&sink, commands);
        sink.Close().ok()?;
        Ok(geometry)
    }
}

/// Feeds `commands` into an open geometry sink.
///
/// Segment commands that appear with no open figure implicitly begin one at
/// the origin, matching the fill path's single-figure fallback; `Arc` begins
/// at its parametric start instead. `MoveTo` ends the current figure open.
fn record_commands(sink: &ID2D1GeometrySink, commands: &[PathCommand]) {
    let mut figure_open = false;
    macro_rules! begin {
        () => {
            if !figure_open {
                unsafe { sink.BeginFigure(point(0.0, 0.0), D2D1_FIGURE_BEGIN_FILLED) };
                figure_open = true;
            }
        };
    }
    for command in commands {
        match *command {
            PathCommand::MoveTo { x, y } => {
                if figure_open {
                    unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
                }
                unsafe { sink.BeginFigure(point(x, y), D2D1_FIGURE_BEGIN_FILLED) };
                figure_open = true;
            }
            PathCommand::LineTo { x, y } => {
                begin!();
                unsafe { sink.AddLine(point(x, y)) };
            }
            PathCommand::QuadTo { cx, cy, x, y } => {
                begin!();
                unsafe {
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: point(cx, cy),
                        point2: point(x, y),
                    });
                };
            }
            PathCommand::CubicTo {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => {
                begin!();
                unsafe {
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: point(c1x, c1y),
                        point2: point(c2x, c2y),
                        point3: point(x, y),
                    });
                };
            }
            PathCommand::Arc {
                cx,
                cy,
                rx,
                ry,
                start,
                sweep,
            } => record_arc(sink, &mut figure_open, cx, cy, rx, ry, start, sweep),
            PathCommand::Close => {
                if figure_open {
                    unsafe { sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
                    figure_open = false;
                }
            }
        }
    }
    if figure_open {
        unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
    }
}

/// Records a center-parametrized `WaterUI` arc.
///
/// `AddArc` is endpoint-parametrized, so the arc's start point is materialized
/// first: an open figure gets a connector line to the arc start (matching the
/// Apple backend's `CGPath` behavior), while an empty figure begins there.
/// A sweep of a full turn or more cannot be expressed by an endpoint arc, so
/// it becomes a closed ellipse emitted as two semicircle arcs.
#[allow(clippy::too_many_arguments)]
fn record_arc(
    sink: &ID2D1GeometrySink,
    figure_open: &mut bool,
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    start: f32,
    sweep: f32,
) {
    let at = |angle: f32| point(cx + rx * angle.cos(), cy + ry * angle.sin());
    let direction = if sweep >= 0.0 {
        D2D1_SWEEP_DIRECTION_CLOCKWISE
    } else {
        D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
    };
    let segment = |end: D2D1_POINT_2F, arcsize| D2D1_ARC_SEGMENT {
        point: end,
        size: D2D1_SIZE_F {
            width: rx,
            height: ry,
        },
        rotationangle: 0.0,
        sweepdirection: direction,
        arcsize,
    };

    if sweep.abs() >= 2.0 * PI {
        if *figure_open {
            unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
            *figure_open = false;
        }
        let start_point = at(start);
        unsafe { sink.BeginFigure(start_point, D2D1_FIGURE_BEGIN_FILLED) };
        unsafe { sink.AddArc(&segment(at(start + PI), D2D1_ARC_SIZE_SMALL)) };
        unsafe { sink.AddArc(&segment(start_point, D2D1_ARC_SIZE_SMALL)) };
        unsafe { sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
    } else {
        let start_point = at(start);
        if *figure_open {
            unsafe { sink.AddLine(start_point) };
        } else {
            unsafe { sink.BeginFigure(start_point, D2D1_FIGURE_BEGIN_FILLED) };
            *figure_open = true;
        }
        let arcsize = if sweep.abs() > PI {
            D2D1_ARC_SIZE_LARGE
        } else {
            D2D1_ARC_SIZE_SMALL
        };
        unsafe { sink.AddArc(&segment(at(start + sweep), arcsize)) };
    }
}
