//! Graphics views: colors, gradients, shapes, icons, GPU surfaces.

use waterui_core::{Environment, Native};
use waterui_graphics::Gradient;
use waterui_graphics::color::Color;
use waterui_graphics::draw::{ColorStop, Extend, Interpolation, Paint, RadialGradient};
use waterui_icon::SystemIcon;
use waterui_shape::{PathCommand, ResolvedShape};
use windows_core::Interface;

#[cfg(feature = "gpu")]
use waterui_graphics::GpuContentView;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{solid_brush, store_watcher_guard, subscribe_then_get};

/// A filled `Border` is the color swatch; stretch is `Both` by contract.
fn color_swatch() -> Border {
    Border::new().expect("Border::new")
}

impl WinUiComponent for Native<Color> {
    /// Fills a `Border` with the resolved color, re-resolving on theme or
    /// signal change.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let resolved = self.into_inner().resolve(env);
        let swatch = color_swatch();
        let queue = renderer.executor().queue().clone();
        let weak = swatch.downgrade().expect("Border supports weak refs");
        let (initial, guard) = subscribe_then_get(&resolved, move |ctx| {
            let color = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(swatch) = weak.upgrade() {
                    swatch
                        .SetBackground(&solid_brush(&color).expect("SolidColorBrush"))
                        .expect("Border::SetBackground");
                }
            });
        });
        swatch
            .SetBackground(&solid_brush(&initial).expect("SolidColorBrush"))
            .expect("Border::SetBackground");
        let element: FrameworkElement = swatch.cast().expect("Border is a FrameworkElement");
        store_watcher_guard(&element, guard);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for Native<Gradient> {
    /// Fills a `Border` with a `LinearGradientBrush` or `RadialGradientBrush`.
    ///
    /// Sweep and mesh gradients have no `WinUI` brush realization; they
    /// panic rather than silently degrading.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "normalized gradient coordinates fit f32"
    )]
    fn render(self, _env: &Environment, _renderer: &mut WinUiRenderer) -> UIElement {
        let gradient = self.into_inner();
        let swatch = color_swatch();

        let brush: Brush = match gradient.paint() {
            Paint::Linear(linear) => {
                let brush = LinearGradientBrush::new().expect("LinearGradientBrush::new");
                brush
                    .SetStartPoint(Point {
                        x: linear.start.x as f32,
                        y: linear.start.y as f32,
                    })
                    .expect("LinearGradientBrush::SetStartPoint");
                brush
                    .SetEndPoint(Point {
                        x: linear.end.x as f32,
                        y: linear.end.y as f32,
                    })
                    .expect("LinearGradientBrush::SetEndPoint");
                let gradient_brush = brush
                    .cast::<IGradientBrush>()
                    .expect("LinearGradientBrush is an IGradientBrush");
                gradient_brush
                    .SetColorInterpolationMode(color_interpolation_mode(linear.interpolation))
                    .expect("IGradientBrush::SetColorInterpolationMode");
                gradient_brush
                    .SetSpreadMethod(spread_method(linear.extend))
                    .expect("IGradientBrush::SetSpreadMethod");
                append_gradient_stops(
                    &gradient_brush
                        .GradientStops()
                        .expect("IGradientBrush::GradientStops"),
                    &linear.stops,
                );
                brush.cast().expect("LinearGradientBrush is a Brush")
            }
            Paint::Radial(radial) => {
                // `Gradient::radial` always produces equal centres; the WinUI
                // brush has a single centre.
                assert!(
                    radial.start_center == radial.end_center,
                    "a radial gradient's centres must coincide"
                );
                let brush = RadialGradientBrush::new().expect("RadialGradientBrush::new");
                brush
                    .SetCenter(Point {
                        x: radial.start_center.x as f32,
                        y: radial.start_center.y as f32,
                    })
                    .expect("RadialGradientBrush::SetCenter");
                brush
                    .SetGradientOrigin(Point {
                        x: radial.start_center.x as f32,
                        y: radial.start_center.y as f32,
                    })
                    .expect("RadialGradientBrush::SetGradientOrigin");
                brush
                    .SetInterpolationSpace(interpolation_space(radial.interpolation))
                    .expect("RadialGradientBrush::SetInterpolationSpace");
                brush
                    .SetSpreadMethod(spread_method(radial.extend))
                    .expect("RadialGradientBrush::SetSpreadMethod");
                // The brush radius is the outer of the two circles
                // (`Gradient::radial` allows `start_radius > end_radius`).
                let radius = radial.start_radius.max(radial.end_radius);
                brush
                    .SetRadiusX(radius)
                    .expect("RadialGradientBrush::SetRadiusX");
                brush
                    .SetRadiusY(radius)
                    .expect("RadialGradientBrush::SetRadiusY");
                append_gradient_stops(
                    &brush
                        .cast::<IRadialGradientBrush>()
                        .expect("RadialGradientBrush is an IRadialGradientBrush")
                        .GradientStops()
                        .expect("IRadialGradientBrush::GradientStops"),
                    &radial_stops(radial),
                );
                brush.cast().expect("RadialGradientBrush is a Brush")
            }
            Paint::Sweep(_) | Paint::Mesh(_) => {
                panic!("sweep and mesh gradients have no WinUI brush realization")
            }
            _ => panic!("a native gradient must carry a gradient paint"),
        };
        swatch.SetBackground(&brush).expect("Border::SetBackground");
        swatch.cast().expect("Border is a UIElement")
    }
}

/// Remaps a radial gradient's stops onto the `RadialGradientBrush` ramp, in
/// ascending offset order.
///
/// The brush ramp runs from the centre (offset 0) to the outer radius `R`
/// (offset 1), so radial offset `o` in `[r0, r1]` becomes brush offset
/// `(r0 + o * (r1 - r0)) / R`, the identity when `r0 = 0`.
///
/// # Panics
/// On `Extend::Repeat` or `Extend::Reflect` when the smaller radius is above
/// 0: the brush repeats its ramp over `[0, R]` from the centre, while the
/// gradient repeats over `[r0, r1]`, so both the period and the phase would
/// be wrong. Also when the stops do not ascend by offset, the
/// `RadialGradient` contract.
#[expect(
    clippy::cast_possible_truncation,
    reason = "normalized gradient offsets fit f32"
)]
fn radial_stops(radial: &RadialGradient) -> Vec<ColorStop> {
    let inner = radial.start_radius.min(radial.end_radius);
    assert!(
        !(matches!(radial.extend, Extend::Repeat | Extend::Reflect) && inner > 0.0),
        "radial gradient extend mode `{:?}` with a smaller radius of {inner} has no WinUI realization: WinUI's RadialGradientBrush repeats from the centre, not from the inner circle",
        radial.extend
    );
    assert!(
        radial.stops.is_sorted_by(|a, b| a.offset <= b.offset),
        "a radial gradient's stops must ascend by offset"
    );
    let radius = radial.start_radius.max(radial.end_radius);
    let span = radial.end_radius - radial.start_radius;
    let mut stops: Vec<ColorStop> = radial
        .stops
        .iter()
        .map(|stop| ColorStop {
            offset: ((radial.start_radius + f64::from(stop.offset) * span) / radius) as f32,
            ..*stop
        })
        .collect();
    // A shrinking ramp (`r0 > r1`) maps ascending stops to descending
    // offsets. Reversing rather than sorting keeps coincident (hard) stops
    // in the order that puts each colour on its own side of the edge.
    if span < 0.0 {
        stops.reverse();
    }
    stops
}

/// `LinearGradientBrush` interpolates in gamma-encoded sRGB unless told
/// otherwise; `ScRgbLinearInterpolation` is the linear working space, exact
/// only for sRGB-in-gamut stops: `working_color_to_winui` clips wider-gamut
/// (e.g. Display P3) stops to 8-bit sRGB.
const fn color_interpolation_mode(interpolation: Interpolation) -> ColorInterpolationMode {
    match interpolation {
        Interpolation::Working => ColorInterpolationMode::ScRgbLinearInterpolation,
        Interpolation::SrgbEncoded => ColorInterpolationMode::SRgbLinearInterpolation,
    }
}

/// `RadialGradientBrush` is a composition brush: its interpolation space is a
/// `CompositionColorSpace`, not the `GradientBrush` `ColorInterpolationMode`.
const fn interpolation_space(interpolation: Interpolation) -> CompositionColorSpace {
    match interpolation {
        Interpolation::Working => CompositionColorSpace::RgbLinear,
        Interpolation::SrgbEncoded => CompositionColorSpace::Rgb,
    }
}

/// # Panics
/// On [`Extend::None`]: a `WinUI` gradient brush always paints past its
/// range, so it cannot be transparent there.
fn spread_method(extend: Extend) -> GradientSpreadMethod {
    match extend {
        Extend::Pad => GradientSpreadMethod::Pad,
        Extend::Repeat => GradientSpreadMethod::Repeat,
        Extend::Reflect => GradientSpreadMethod::Reflect,
        Extend::None => panic!(
            "gradient extend mode `Extend::None` (transparent outside the range) has no WinUI GradientSpreadMethod"
        ),
    }
}

/// `GradientStops` is `GradientStopCollection` on `IGradientBrush` but
/// `IObservableVector<GradientStop>` on `IRadialGradientBrush`; both are
/// `IVector<GradientStop>` at the ABI.
fn append_gradient_stops<C: Interface>(stops: &C, gradient_stops: &[ColorStop]) {
    let vector = crate::util::vector::<_, GradientStop>(stops);
    for stop in gradient_stops {
        let native = GradientStop::new().expect("GradientStop::new");
        native
            .SetOffset(f64::from(stop.offset))
            .expect("GradientStop::SetOffset");
        native
            .SetColor(crate::util::working_color_to_winui(&stop.color))
            .expect("GradientStop::SetColor");
        vector
            .Append(&native)
            .expect("GradientStopCollection::Append");
    }
}

impl WinUiComponent for Native<ResolvedShape> {
    /// Renders a `Path` from the resolved shape's command list.
    fn render(self, _env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let shape = self.into_inner();
        let path = Path::new().expect("Path::new");
        // Fill/stretch live on `IShape`; `SetData` is `IPath` (deref target).
        let shape_el = path.cast::<IShape>().expect("Path is a Shape");
        shape_el
            .SetStretch(Stretch::Fill)
            .expect("IShape::SetStretch");

        let geometry = build_path_geometry(&shape);
        path.SetData(&geometry).expect("Path::SetData");

        let fill = shape.fill;
        let queue = renderer.executor().queue().clone();
        let weak = shape_el.downgrade().expect("IShape supports weak refs");
        let (initial, guard) = subscribe_then_get(&fill, move |ctx| {
            let color = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(shape) = weak.upgrade() {
                    shape
                        .SetFill(&solid_brush(&color).expect("SolidColorBrush"))
                        .expect("IShape::SetFill");
                }
            });
        });
        shape_el
            .SetFill(&solid_brush(&initial).expect("SolidColorBrush"))
            .expect("IShape::SetFill");

        let element: FrameworkElement = path.cast().expect("Path is a FrameworkElement");
        store_watcher_guard(&element, guard);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

/// Builds a `PathGeometry` from `WaterUI` path commands.
///
/// `WaterUI` path coordinates are normalized to the shape's bounds (0.0–1.0);
/// the `Path` is stretched to `Fill`, so the geometry is emitted in the unit
/// square and XAML scales it to the arranged size.
///
/// The translation mirrors the Direct2D clip recorder in [`crate::d2d`]: each
/// `MoveTo` ends the current figure and opens a new one, segment commands
/// with no open figure implicitly start one at the origin, and
/// center-parametrized `Arc` commands become endpoint-parametrized
/// `ArcSegment`s — an open figure gets a connector line to the arc's
/// parametric start (the `CGPath` behavior the Apple backend follows), while
/// a sweep of a full turn or more is emitted as two semicircle arcs since an
/// endpoint arc cannot express a complete ellipse.
#[allow(clippy::too_many_lines)] // one branch per `PathCommand`
fn build_path_geometry(shape: &ResolvedShape) -> PathGeometry {
    let geometry = PathGeometry::new().expect("PathGeometry::new");
    let figures =
        crate::util::vector::<_, PathFigure>(&geometry.Figures().expect("PathGeometry::Figures"));

    let mut figure = PathFigure::new().expect("PathFigure::new");
    let mut segments =
        crate::util::vector::<_, PathSegment>(&figure.Segments().expect("PathFigure::Segments"));
    let mut figure_open = false;
    let mut figure_start = Point { x: 0.0, y: 0.0 };
    let mut current = Point { x: 0.0, y: 0.0 };

    macro_rules! ensure_figure {
        () => {
            if !figure_open {
                figure
                    .SetStartPoint(current)
                    .expect("PathFigure::SetStartPoint");
                figure_start = current;
                figure_open = true;
            }
        };
    }
    macro_rules! push_segment {
        ($segment:expr) => {
            segments
                .Append(&$segment.cast::<PathSegment>().expect("PathSegment"))
                .expect("PathSegmentCollection::Append")
        };
    }

    for command in &shape.commands {
        match *command {
            PathCommand::MoveTo { x, y } => {
                if figure_open {
                    figures
                        .Append(&figure)
                        .expect("PathFigureCollection::Append");
                    figure = PathFigure::new().expect("PathFigure::new");
                    segments = crate::util::vector::<_, PathSegment>(
                        &figure.Segments().expect("PathFigure::Segments"),
                    );
                }
                figure
                    .SetStartPoint(Point { x, y })
                    .expect("PathFigure::SetStartPoint");
                figure_start = Point { x, y };
                current = Point { x, y };
                figure_open = true;
            }
            PathCommand::LineTo { x, y } => {
                ensure_figure!();
                let segment = LineSegment::new().expect("LineSegment::new");
                segment
                    .SetPoint(Point { x, y })
                    .expect("LineSegment::SetPoint");
                push_segment!(segment);
                current = Point { x, y };
            }
            PathCommand::QuadTo { cx, cy, x, y } => {
                ensure_figure!();
                let segment = QuadraticBezierSegment::new().expect("QuadraticBezierSegment::new");
                segment
                    .SetPoint1(Point { x: cx, y: cy })
                    .expect("QuadraticBezierSegment::SetPoint1");
                segment
                    .SetPoint2(Point { x, y })
                    .expect("QuadraticBezierSegment::SetPoint2");
                push_segment!(segment);
                current = Point { x, y };
            }
            PathCommand::CubicTo {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => {
                ensure_figure!();
                let segment = BezierSegment::new().expect("BezierSegment::new");
                segment
                    .SetPoint1(Point { x: c1x, y: c1y })
                    .expect("BezierSegment::SetPoint1");
                segment
                    .SetPoint2(Point { x: c2x, y: c2y })
                    .expect("BezierSegment::SetPoint2");
                segment
                    .SetPoint3(Point { x, y })
                    .expect("BezierSegment::SetPoint3");
                push_segment!(segment);
                current = Point { x, y };
            }
            PathCommand::Arc {
                cx,
                cy,
                rx,
                ry,
                start,
                sweep,
            } => {
                let at = |angle: f32| Point {
                    x: cx + rx * angle.cos(),
                    y: cy + ry * angle.sin(),
                };
                let arc_start = at(start);
                if figure_open {
                    if current != arc_start {
                        let connector = LineSegment::new().expect("LineSegment::new");
                        connector
                            .SetPoint(arc_start)
                            .expect("LineSegment::SetPoint");
                        push_segment!(connector);
                    }
                } else {
                    figure
                        .SetStartPoint(arc_start)
                        .expect("PathFigure::SetStartPoint");
                    figure_start = arc_start;
                    figure_open = true;
                }
                let direction = if sweep >= 0.0 {
                    SweepDirection::Clockwise
                } else {
                    SweepDirection::Counterclockwise
                };
                let push_arc = |end: Point, is_large: bool| {
                    let segment = ArcSegment::new().expect("ArcSegment::new");
                    segment.SetPoint(end).expect("ArcSegment::SetPoint");
                    segment
                        .SetSize(Size {
                            width: rx,
                            height: ry,
                        })
                        .expect("ArcSegment::SetSize");
                    segment
                        .SetIsLargeArc(is_large)
                        .expect("ArcSegment::SetIsLargeArc");
                    segment
                        .SetSweepDirection(direction)
                        .expect("ArcSegment::SetSweepDirection");
                    push_segment!(segment);
                };
                if sweep.abs() >= 2.0 * core::f32::consts::PI {
                    push_arc(at(start + core::f32::consts::PI), false);
                    push_arc(arc_start, false);
                    figure.SetIsClosed(true).expect("PathFigure::SetIsClosed");
                    current = arc_start;
                } else {
                    let end = at(start + sweep);
                    push_arc(end, sweep.abs() > core::f32::consts::PI);
                    current = end;
                }
            }
            PathCommand::Close => {
                if figure_open {
                    figure.SetIsClosed(true).expect("PathFigure::SetIsClosed");
                    current = figure_start;
                }
            }
        }
    }
    if figure_open {
        figures
            .Append(&figure)
            .expect("PathFigureCollection::Append");
    }
    geometry
}

impl WinUiComponent for Native<SystemIcon> {
    /// Maps the SF-Symbols-style name to a `SymbolIcon` glyph.
    ///
    /// Windows' Segoe Fluent Icons catalog is a real OS icon catalog, so
    /// `SystemIcon` is supported — names that have no catalog entry panic
    /// rather than silently substituting a bundled font.
    fn render(self, _env: &Environment, _renderer: &mut WinUiRenderer) -> UIElement {
        let icon = self.into_inner();
        let symbol = segoe_symbol_for(icon.name.as_str()).unwrap_or_else(|| {
            panic!(
                "SystemIcon `{}` has no Segoe Fluent Icons equivalent; \
                 use a packaged icon crate (waterui-icons-lucide, …)",
                icon.name.as_str()
            )
        });
        let element = SymbolIcon::new().expect("SymbolIcon::new");
        element.SetSymbol(symbol).expect("SymbolIcon::SetSymbol");
        element.cast().expect("SymbolIcon is a UIElement")
    }
}

/// SF Symbol name → `WinUI` `Symbol` glyph, for the names `WaterUI`'s
/// `system_icon::*` constructors produce.
#[allow(clippy::match_same_arms)] // a lookup table: distinct SF names share glyphs
pub(crate) fn segoe_symbol_for(name: &str) -> Option<Symbol> {
    Some(match name {
        "house" | "house.fill" => Symbol::Home,
        "gear" | "gearshape" | "gearshape.fill" => Symbol::Setting,
        "magnifyingglass" => Symbol::Find,
        "plus" => Symbol::Add,
        "minus" => Symbol::Remove,
        "xmark" => Symbol::Cancel,
        "checkmark" => Symbol::Accept,
        "chevron.left" => Symbol::Back,
        "chevron.right" => Symbol::Forward,
        "chevron.up" => Symbol::Up,
        "chevron.down" => Symbol::Download,
        "pencil" => Symbol::Edit,
        "trash" | "trash.fill" => Symbol::Delete,
        "square.and.arrow.up" => Symbol::Share,
        "heart" | "heart.fill" => Symbol::Favorite,
        "bell" | "bell.fill" => Symbol::Important,
        "person" | "person.fill" => Symbol::Contact,
        "envelope" | "envelope.fill" => Symbol::Mail,
        "calendar" => Symbol::Calendar,
        "clock" => Symbol::Clock,
        "camera" | "camera.fill" => Symbol::Camera,
        "photo" => Symbol::Pictures,
        "folder" | "folder.fill" => Symbol::Folder,
        "doc" | "doc.fill" => Symbol::Document,
        "arrow.clockwise" => Symbol::Refresh,
        "play" | "play.fill" => Symbol::Play,
        "pause" | "pause.fill" => Symbol::Pause,
        "stop" | "stop.fill" => Symbol::Stop,
        "speaker" | "speaker.fill" => Symbol::Volume,
        "speaker.slash" => Symbol::Mute,
        "wifi" => Symbol::FourBars,
        "lock" | "lock.fill" => Symbol::Permissions,
        "eye" | "eye.fill" => Symbol::View,
        "info.circle" | "questionmark.circle" => Symbol::Help,
        "exclamationmark.triangle" => Symbol::Important,
        "flag" | "flag.fill" => Symbol::Flag,
        "bookmark" | "bookmark.fill" => Symbol::Bookmarks,
        "tag" | "tag.fill" => Symbol::Tag,
        "mappin" | "mappin.and.ellipse" => Symbol::MapPin,
        "globe" => Symbol::Globe,
        "star" => Symbol::OutlineStar,
        "star.fill" => Symbol::SolidStar,
        _ => return None,
    })
}

#[cfg(feature = "gpu")]
impl WinUiComponent for Native<GpuContentView> {
    /// Hosts the content on a `SwapChainPanel`.
    ///
    /// The full wgpu/swapchain interop is provided by `crate::gpu`; this
    /// handler creates the panel and starts the render loop.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        crate::gpu::render_gpu_content(renderer, self.into_inner(), env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "WinUI's RadialGradientBrush repeats from the centre")]
    fn radial_repeat_with_a_nonzero_smaller_radius_panics() {
        let radial =
            RadialGradient::two_point((0.5, 0.5), 0.2, (0.5, 0.5), 0.5).extend(Extend::Repeat);
        radial_stops(&radial);
    }
}
