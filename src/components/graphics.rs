//! Graphics views: colors, gradients, shapes, icons, GPU surfaces.

use waterui_core::{Environment, Native};
use waterui_graphics::color::{Color, ResolvedColor};
use waterui_icon::SystemIcon;
use waterui_shape::{PathCommand, ResolvedShape};
use windows_core::Interface;

#[cfg(feature = "gpu")]
use waterui_graphics::ResolvedGradient;
#[cfg(feature = "gpu")]
use waterui_graphics::gpu_surface::GpuSurface;

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

impl WinUiComponent for Native<ResolvedColor> {
    /// Fills a `Border` with the resolved color.
    fn render(self, _env: &Environment, _renderer: &mut WinUiRenderer) -> UIElement {
        let swatch = color_swatch();
        swatch
            .SetBackground(&solid_brush(&self.into_inner()).expect("SolidColorBrush"))
            .expect("Border::SetBackground");
        swatch.cast().expect("Border is a UIElement")
    }
}

#[cfg(feature = "gpu")]
impl WinUiComponent for Native<ResolvedGradient> {
    /// Fills a `Border` with a `LinearGradientBrush` or `RadialGradientBrush`.
    ///
    /// Angular and mesh gradients have no WinUI brush equivalent; they require
    /// the GPU (filtrate) path and panic rather than silently degrading.
    fn render(self, _env: &Environment, _renderer: &mut WinUiRenderer) -> UIElement {
        use waterui_graphics::GradientType;

        let gradient = self.into_inner();
        let swatch = color_swatch();

        let brush: Brush = match gradient.gradient_type {
            GradientType::Linear => {
                let brush = LinearGradientBrush::new().expect("LinearGradientBrush::new");
                brush
                    .SetStartPoint(Point {
                        x: gradient.start_point[0],
                        y: gradient.start_point[1],
                    })
                    .expect("LinearGradientBrush::SetStartPoint");
                brush
                    .SetEndPoint(Point {
                        x: gradient.end_point[0],
                        y: gradient.end_point[1],
                    })
                    .expect("LinearGradientBrush::SetEndPoint");
                append_gradient_stops(
                    &brush
                        .cast::<IGradientBrush>()
                        .expect("LinearGradientBrush is an IGradientBrush")
                        .GradientStops()
                        .expect("IGradientBrush::GradientStops"),
                    &gradient.stops,
                );
                brush.cast().expect("LinearGradientBrush is a Brush")
            }
            GradientType::Radial => {
                let brush = RadialGradientBrush::new().expect("RadialGradientBrush::new");
                brush
                    .SetCenter(Point {
                        x: gradient.start_point[0],
                        y: gradient.start_point[1],
                    })
                    .expect("RadialGradientBrush::SetCenter");
                brush
                    .SetGradientOrigin(Point {
                        x: gradient.start_point[0],
                        y: gradient.start_point[1],
                    })
                    .expect("RadialGradientBrush::SetGradientOrigin");
                brush
                    .SetRadiusX(f64::from(gradient.start_value))
                    .expect("RadialGradientBrush::SetRadiusX");
                brush
                    .SetRadiusY(f64::from(gradient.end_value))
                    .expect("RadialGradientBrush::SetRadiusY");
                append_gradient_stops(
                    &brush
                        .cast::<IGradientBrush>()
                        .expect("RadialGradientBrush is an IGradientBrush")
                        .GradientStops()
                        .expect("IGradientBrush::GradientStops"),
                    &gradient.stops,
                );
                brush.cast().expect("RadialGradientBrush is a Brush")
            }
            GradientType::Angular | GradientType::Mesh => panic!(
                "angular and mesh gradients have no WinUI brush realization; \
                 they require the GPU filter path"
            ),
        };
        swatch.SetBackground(&brush).expect("Border::SetBackground");
        swatch.cast().expect("Border is a UIElement")
    }
}

#[cfg(feature = "gpu")]
fn append_gradient_stops(
    stops: &GradientStopCollection,
    gradient_stops: &[waterui_graphics::ResolvedGradientStop],
) {
    let vector = crate::util::vector::<_, GradientStop>(stops);
    for stop in gradient_stops {
        let native = GradientStop::new().expect("GradientStop::new");
        native
            .SetOffset(f64::from(stop.position))
            .expect("GradientStop::SetOffset");
        native
            .SetColor(crate::util::resolved_color_to_winui(&stop.color))
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

/// Builds a `PathGeometry` from WaterUI path commands.
///
/// WaterUI path coordinates are normalized to the shape's bounds (0.0–1.0);
/// the `Path` is stretched to `Fill`, so the geometry is emitted in the unit
/// square and XAML scales it to the arranged size.
fn build_path_geometry(shape: &ResolvedShape) -> PathGeometry {
    let geometry = PathGeometry::new().expect("PathGeometry::new");
    let figure = PathFigure::new().expect("PathFigure::new");

    let mut segments = Vec::new();
    for command in &shape.commands {
        match *command {
            PathCommand::MoveTo { x, y } => {
                figure
                    .SetStartPoint(Point { x, y })
                    .expect("PathFigure::SetStartPoint");
            }
            PathCommand::LineTo { x, y } => {
                let segment = LineSegment::new().expect("LineSegment::new");
                segment
                    .SetPoint(Point { x, y })
                    .expect("LineSegment::SetPoint");
                segments.push(segment.cast::<PathSegment>().expect("PathSegment"));
            }
            PathCommand::QuadTo { cx, cy, x, y } => {
                let segment = QuadraticBezierSegment::new().expect("QuadraticBezierSegment::new");
                segment
                    .SetPoint1(Point { x: cx, y: cy })
                    .expect("QuadraticBezierSegment::SetPoint1");
                segment
                    .SetPoint2(Point { x, y })
                    .expect("QuadraticBezierSegment::SetPoint2");
                segments.push(segment.cast::<PathSegment>().expect("PathSegment"));
            }
            PathCommand::CubicTo {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => {
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
                segments.push(segment.cast::<PathSegment>().expect("PathSegment"));
            }
            PathCommand::Arc {
                cx,
                cy,
                rx,
                ry,
                start,
                sweep,
            } => {
                let segment = ArcSegment::new().expect("ArcSegment::new");
                segment
                    .SetPoint(Point {
                        x: cx + rx * (start + sweep).cos(),
                        y: cy + ry * (start + sweep).sin(),
                    })
                    .expect("ArcSegment::SetPoint");
                segment
                    .SetSize(Size {
                        width: rx,
                        height: ry,
                    })
                    .expect("ArcSegment::SetSize");
                segment
                    .SetIsLargeArc(sweep.abs() > core::f32::consts::PI)
                    .expect("ArcSegment::SetIsLargeArc");
                segment
                    .SetSweepDirection(if sweep >= 0.0 {
                        SweepDirection::Clockwise
                    } else {
                        SweepDirection::Counterclockwise
                    })
                    .expect("ArcSegment::SetSweepDirection");
                segments.push(segment.cast::<PathSegment>().expect("PathSegment"));
            }
            PathCommand::Close => {
                figure.SetIsClosed(true).expect("PathFigure::SetIsClosed");
            }
        }
    }

    let segment_collection =
        crate::util::vector::<_, PathSegment>(&figure.Segments().expect("PathFigure::Segments"));
    for segment in segments {
        segment_collection
            .Append(&segment)
            .expect("PathSegmentCollection::Append");
    }
    crate::util::vector::<_, PathFigure>(&geometry.Figures().expect("PathGeometry::Figures"))
        .Append(&figure)
        .expect("PathFigureCollection::Append");
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

/// SF Symbol name → WinUI `Symbol` glyph, for the names WaterUI's
/// `system_icon::*` constructors produce.
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
impl WinUiComponent for Native<GpuSurface> {
    /// Hosts the `GpuView` on a `SwapChainPanel`.
    ///
    /// The full wgpu/swapchain interop is provided by
    /// `crate::gpu::GpuSurfaceHost`; this handler creates the panel and
    /// starts the render loop.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        crate::gpu::render_gpu_surface(renderer, self.into_inner(), env)
    }
}
