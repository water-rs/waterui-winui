//! GPU surface hosting: `GpuSurface` renders through wgpu into a
//! `SwapChainPanel`, and `AppliedFilter` captures the wrapped element with
//! `RenderTargetBitmap`, runs the filtrate effect on the GPU, and presents the
//! result into an overlaying `SwapChainPanel`.
//!
//! The capture path reads the filtered subtree back through the compositor
//! once per frame; the direct `GpuSurface` path presents without a readback.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use executor_core::{LocalExecutor, Task};
use send_wrapper::SendWrapper;
use waterui_core::Environment;
use waterui_core::Metadata;
use waterui_core::layout::Point;
use waterui_graphics::gpu_surface::{GestureState, GpuSurface, PointerState};
use waterui_graphics::{
    AppliedFilter, EffectFrameClock, EffectInput, EffectOutput, GpuContext, GpuRuntime, wgpu,
};
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::framework;

/// Shared per-surface state mutated from event handlers and the render pump.
struct SurfaceState {
    /// The `WaterUI` view driving this surface.
    view: RefCell<GpuSurface>,
    /// Latest pointer/gesture snapshot fed into `GpuFrame`.
    pointer: Cell<Point>,
    pointer_hovering: Cell<bool>,
    gesture: RefCell<GestureState>,
    /// Physical pixel size of the swapchain.
    size: Cell<(u32, u32)>,
    /// Whether the view finished `GpuView::setup`.
    ready: Cell<bool>,
    /// Whether a frame is already queued.
    frame_pending: Cell<bool>,
    /// Clock feeding `GpuFrame::elapsed`/`delta`.
    last_frame: Cell<Option<Instant>>,
    start: Instant,
}

/// Renders a `GpuSurface` view into a `SwapChainPanel`.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::await_holding_refcell_ref
)]
// Flat wiring; pixel conversions saturate intentionally via `as` after
// `.max(1.0)`; `view` has exactly one borrower on the UI dispatcher.
pub(crate) fn render_gpu_surface(
    renderer: &WinUiRenderer,
    surface: GpuSurface,
    env: &Environment,
) -> UIElement {
    let runtime = env
        .get::<GpuRuntime>()
        .expect("GpuRuntime missing from Environment; waterui-winui app startup installs it")
        .clone();

    let panel = SwapChainPanel::new().expect("SwapChainPanel::new");
    let element: UIElement = panel.cast().expect("SwapChainPanel is a UIElement");

    let native: ISwapChainPanelNative = panel.cast().expect("ISwapChainPanelNative");
    let wgpu_surface = Rc::new(
        unsafe {
            runtime
                .context()
                .instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::SwapChainPanel(native.as_raw()))
        }
        .expect("wgpu surface creation from SwapChainPanel failed"),
    );

    let context = runtime.context();
    let format = pick_surface_format(
        &wgpu_surface,
        &context.adapter,
        surface.resolved_hdr_preference(),
    );
    let max_samples = surface.msaa_sample_limit();

    let redraw_handle = waterui_graphics::gpu_surface::RedrawHandle::new();
    let queue = renderer.executor().queue().clone();

    let state = Rc::new(SurfaceState {
        view: RefCell::new(surface),
        pointer: Cell::new(Point::new(0.0, 0.0)),
        pointer_hovering: Cell::new(false),
        gesture: RefCell::new(GestureState::new()),
        size: Cell::new((0, 0)),
        ready: Cell::new(false),
        frame_pending: Cell::new(false),
        last_frame: Cell::new(None),
        start: Instant::now(),
    });

    // Waking a redraw hops back to the UI thread and pumps one frame.
    // `RedrawWaker` is `Send + Sync` because the view may request a redraw from
    // any thread; the UI-bound state crosses the hop inside `SendWrapper`s and
    // is only dereferenced on the dispatcher thread.
    {
        let queue = queue.clone();
        let state = std::sync::Arc::new(SendWrapper::new(Rc::downgrade(&state)));
        let wgpu_surface = std::sync::Arc::new(SendWrapper::new(wgpu_surface.clone()));
        let runtime = SendWrapper::new(runtime.clone());
        redraw_handle.set_waker(Some(std::sync::Arc::new(move || {
            let queue = queue.clone();
            let state = state.clone();
            let wgpu_surface = wgpu_surface.clone();
            let runtime = runtime.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(state) = state.upgrade() {
                    pump_frame(&wgpu_surface, &runtime, &state, format);
                }
            });
        })));
    }

    // Setup runs on the dispatcher: `GpuView::setup` is a main-thread future.
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let mut env = env.clone();
        renderer.executor().spawn_local(async move {
            // `context()` returns an owned `Arc`: the runtime may swap in a
            // rebuilt context after device loss, so each frame of work pins
            // one generation by holding it in a local.
            let context = runtime.context();
            let ctx = GpuContext::new(
                &context.adapter,
                &context.device,
                &context.queue,
                format,
                &context.shader_cache,
                context.scene_renderer(),
                max_samples,
                redraw_handle,
            );
            // `view` has exactly one borrower on the single-threaded UI dispatcher.
            state.view.borrow_mut().setup(&ctx, &mut env).await;
            state.ready.set(true);
        });
    }

    let fe = framework(&element);

    // Resize reconfigures the swapchain and redraws.
    {
        let state = state.clone();
        let wgpu_surface = wgpu_surface.clone();
        let runtime = runtime.clone();
        let revoker = fe
            .SizeChanged(move |sender, args| {
                let Ok(sender) = sender.ok() else {
                    return;
                };
                let Ok(args) = args.ok() else {
                    return;
                };
                let element: UIElement = sender.cast().expect("UIElement");
                let new_size = args.NewSize().expect("SizeChangedEventArgs::NewSize");
                let scale = rasterization_scale(&element);
                let w = (f64::from(new_size.width) * scale).max(1.0) as u32;
                let h = (f64::from(new_size.height) * scale).max(1.0) as u32;
                if state.size.get() != (w, h) {
                    state.size.set((w, h));
                    configure(&wgpu_surface, &runtime, format, w, h);
                }
                pump_frame(&wgpu_surface, &runtime, &state, format);
            })
            .expect("FrameworkElement::SizeChanged");
        crate::util::store_event_revoker(&fe, revoker);
    }

    // Pointer tracking feeds `GpuFrame::pointer`.
    {
        let state = state.clone();
        let revoker = element
            .PointerMoved(move |sender, args| {
                let Ok(sender) = sender.ok() else {
                    return;
                };
                let Ok(args) = args.ok() else {
                    return;
                };
                let element: UIElement = sender.cast().expect("UIElement");
                let point = args
                    .GetCurrentPoint(&element)
                    .expect("GetCurrentPoint")
                    .Position()
                    .expect("PointerPoint::Position");
                let scale = rasterization_scale(&element);
                state
                    .pointer
                    .set(Point::new(point.x * scale as f32, point.y * scale as f32));
                state.pointer_hovering.set(true);
            })
            .expect("UIElement::PointerMoved");
        crate::util::store_event_revoker(&fe, revoker);
    }
    {
        let state = state.clone();
        let revoker = element
            .PointerExited(move |_sender, _args| {
                state.pointer_hovering.set(false);
            })
            .expect("UIElement::PointerExited");
        crate::util::store_event_revoker(&fe, revoker);
    }

    element
}

fn rasterization_scale(element: &UIElement) -> f64 {
    element
        .XamlRoot()
        .and_then(|root| root.RasterizationScale())
        .unwrap_or(1.0)
}

fn pick_surface_format(
    surface: &wgpu::Surface<'static>,
    adapter: &wgpu::Adapter,
    hdr: Option<bool>,
) -> wgpu::TextureFormat {
    let caps = surface.get_capabilities(adapter);
    let wants_hdr = hdr.unwrap_or(false);
    caps.formats
        .iter()
        .copied()
        .find(|format| {
            let is_hdr = matches!(
                format,
                wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rgba32Float
            );
            is_hdr == wants_hdr
        })
        .or_else(|| caps.formats.first().copied())
        .expect("adapter exposes no surface formats")
}

fn configure(
    surface: &wgpu::Surface<'static>,
    runtime: &GpuRuntime,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) {
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        format,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
    };
    surface.configure(&runtime.context().device, &config);
}

/// Renders one frame when the surface is configured, the view is ready, and no
/// frame is already pending.
fn pump_frame(
    surface: &Rc<wgpu::Surface<'static>>,
    runtime: &GpuRuntime,
    state: &Rc<SurfaceState>,
    format: wgpu::TextureFormat,
) {
    if !state.ready.get() || state.frame_pending.get() {
        return;
    }
    let (width, height) = state.size.get();
    if width == 0 || height == 0 {
        return;
    }
    state.frame_pending.set(true);

    let texture = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(texture)
        | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            configure(surface, runtime, format, width, height);
            state.frame_pending.set(false);
            return;
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
            state.frame_pending.set(false);
            return;
        }
        wgpu::CurrentSurfaceTexture::Validation => {
            panic!("SwapChainPanel frame acquisition raised a validation error")
        }
    };
    let view = texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    let now = Instant::now();
    let delta = state
        .last_frame
        .replace(Some(now))
        .map_or(Duration::ZERO, |last| now - last);
    let elapsed = now - state.start;

    let pointer = PointerState {
        position: state.pointer_hovering.get().then(|| state.pointer.get()),
        hit: None,
    };
    let gesture = GestureState {
        pinch_scale: state.gesture.borrow().pinch_scale,
        pinch_center: state.gesture.borrow().pinch_center,
        pan_offset: state.gesture.borrow().pan_offset,
        double_tap: state.gesture.borrow().double_tap,
        active: state.gesture.borrow().active,
    };

    let context = runtime.context();
    let mut frame = waterui_graphics::gpu_surface::GpuFrame::new(
        &context.device,
        &context.queue,
        &texture.texture,
        view,
        format,
        width,
        height,
        1.0,
        pointer,
        gesture,
        elapsed,
        delta,
    );
    state.view.borrow_mut().render(&mut frame);
    let redraw = frame.was_redraw_requested();
    texture.present();
    state.frame_pending.set(false);

    if redraw {
        // Vsync-aligned next frame: a self-revoking `Rendering` subscription
        // fires once on the compositor's next frame, then unregisters.
        let state = Rc::downgrade(state);
        let surface = surface.clone();
        let runtime = runtime.clone();
        let revoker_slot = Rc::new(RefCell::new(None));
        let slot = revoker_slot.clone();
        let revoker = CompositionTarget::Rendering(move |_sender, _args| {
            // Revoke this subscription so it fires exactly once.
            slot.borrow_mut().take();
            if let Some(state) = state.upgrade() {
                pump_frame(&surface, &runtime, &state, format);
            }
        })
        .expect("CompositionTarget::Rendering");
        *revoker_slot.borrow_mut() = Some(revoker);
    }
}

/// Shared state for the `AppliedFilter` capture/present pump.
struct FilterState {
    filter: RefCell<AppliedFilter>,
    clock: RefCell<EffectFrameClock>,
    busy: Cell<bool>,
    configured: Cell<bool>,
    /// Last physical size the output swapchain was configured with.
    size: Cell<(u32, u32)>,
}

/// `Metadata<AppliedFilter>`: capture the subtree each frame, run the filter,
/// and present into an overlaying `SwapChainPanel`.
pub(crate) fn render_applied_filter(
    renderer: &mut WinUiRenderer,
    metadata: Metadata<AppliedFilter>,
    env: &Environment,
) -> UIElement {
    let content = renderer.render_any(metadata.content, env);
    let filter = metadata.value;

    let runtime = env
        .get::<GpuRuntime>()
        .expect("GpuRuntime missing from Environment")
        .clone();

    let grid = Grid::new().expect("Grid::new");
    crate::util::children(&grid)
        .Append(&content)
        .expect("Grid::Children::Append");

    let panel = SwapChainPanel::new().expect("SwapChainPanel::new");
    let panel_element: UIElement = panel.cast().expect("SwapChainPanel is a UIElement");
    // The filtered output sits on top of the unfiltered subtree.
    panel_element
        .SetIsHitTestVisible(false)
        .expect("UIElement::SetIsHitTestVisible");
    crate::util::children(&grid)
        .Append(&panel_element)
        .expect("Grid::Children::Append");

    let native: ISwapChainPanelNative = panel.cast().expect("ISwapChainPanelNative");
    let wgpu_surface = Rc::new(
        unsafe {
            runtime
                .context()
                .instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::SwapChainPanel(native.as_raw()))
        }
        .expect("wgpu surface creation from SwapChainPanel failed"),
    );

    let state = Rc::new(FilterState {
        filter: RefCell::new(filter),
        clock: RefCell::new(EffectFrameClock::new()),
        busy: Cell::new(false),
        size: Cell::new((0, 0)),
        configured: Cell::new(false),
    });

    let executor = renderer.executor().clone();
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let wgpu_surface = wgpu_surface.clone();
        let content = content.clone();
        let grid = grid.clone();
        // `CompositionTarget::Rendering` already fires on the UI thread once
        // per compositor frame.
        let grid_for_revoker = grid.clone();
        let revoker = CompositionTarget::Rendering(move |_sender, _args| {
            if state.busy.replace(true) {
                return;
            }
            executor
                .spawn_local(filter_frame(
                    content.clone(),
                    grid.clone(),
                    state.clone(),
                    runtime.clone(),
                    wgpu_surface.clone(),
                ))
                .detach();
        })
        .expect("CompositionTarget::Rendering");
        // The static revoker must outlive the grid; pin it to the element.
        crate::util::store_event_revoker(
            &framework(&grid_for_revoker.cast::<UIElement>().expect("UIElement")),
            revoker,
        );
    }

    grid.cast().expect("Grid is a UIElement")
}

#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
// Flat capture/present pump; pixel conversions saturate intentionally.
async fn filter_frame(
    content: UIElement,
    grid: Grid,
    state: Rc<FilterState>,
    runtime: GpuRuntime,
    wgpu_surface: Rc<wgpu::Surface<'static>>,
) {
    // A `Rendering` tick can land before the first arrange; an element with
    // no rendered extent captures an empty bitmap, and wgpu rejects a
    // zero-dimension texture outright.
    let content_fe = framework(&content);
    if content_fe.ActualWidth().expect("ActualWidth") <= 0.0
        || content_fe.ActualHeight().expect("ActualHeight") <= 0.0
    {
        state.busy.set(false);
        return;
    }

    // Size from the grid in physical pixels.
    let grid_element: UIElement = grid.cast().expect("Grid is a UIElement");
    let fe = framework(&grid_element);
    let scale = rasterization_scale(&grid_element);
    let w = (fe.ActualWidth().expect("ActualWidth") * scale).max(1.0) as u32;
    let h = (fe.ActualHeight().expect("ActualHeight") * scale).max(1.0) as u32;
    if state.size.get() != (w, h) {
        state.size.set((w, h));
        configure(
            &wgpu_surface,
            &runtime,
            wgpu::TextureFormat::Bgra8Unorm,
            w,
            h,
        );
        state.configured.set(true);
    }

    let bitmap = RenderTargetBitmap::new().expect("RenderTargetBitmap::new");
    bitmap
        .RenderAsync(&content)
        .expect("RenderTargetBitmap::RenderAsync")
        .await
        .expect("RenderAsync failed");
    let pw = bitmap.PixelWidth().expect("PixelWidth").cast_unsigned();
    let ph = bitmap.PixelHeight().expect("PixelHeight").cast_unsigned();
    // The visual layer lags layout: a nonzero element can still capture
    // empty before its visual reaches the compositor.
    if pw == 0 || ph == 0 {
        state.busy.set(false);
        return;
    }

    let buffer = bitmap
        .GetPixelsAsync()
        .expect("GetPixelsAsync")
        .await
        .expect("GetPixelsAsync failed");

    let reader = DataReader::FromBuffer(&buffer).expect("DataReader::FromBuffer");
    let mut pixels = vec![0u8; buffer.Length().expect("IBuffer::Length") as usize];
    reader
        .ReadBytes(&mut pixels)
        .expect("DataReader::ReadBytes");

    let device = &runtime.context().device;
    let queue_wgpu = &runtime.context().queue;

    let input = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("waterui-winui filter input"),
        size: wgpu::Extent3d {
            width: pw,
            height: ph,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue_wgpu.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &input,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(pw * 4),
            rows_per_image: Some(ph),
        },
        wgpu::Extent3d {
            width: pw,
            height: ph,
            depth_or_array_layers: 1,
        },
    );

    let frame = match wgpu_surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(frame)
        | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            configure(
                &wgpu_surface,
                &runtime,
                wgpu::TextureFormat::Bgra8Unorm,
                w,
                h,
            );
            state.busy.set(false);
            return;
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
            state.busy.set(false);
            return;
        }
        wgpu::CurrentSurfaceTexture::Validation => {
            panic!("SwapChainPanel frame acquisition raised a validation error")
        }
    };
    let output_view = frame
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let input_view = input.create_view(&wgpu::TextureViewDescriptor::default());

    let timing = state.clock.borrow_mut().tick();
    let input_ref = EffectInput {
        device,
        queue: queue_wgpu,
        texture: &input,
        view: input_view,
        format: wgpu::TextureFormat::Bgra8Unorm,
        width: pw,
        height: ph,
        timing,
    };
    let output = EffectOutput {
        device,
        queue: queue_wgpu,
        texture: &frame.texture,
        view: output_view,
        format: wgpu::TextureFormat::Bgra8Unorm,
        width: w,
        height: h,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("waterui-winui filter encoder"),
    });
    let result = state
        .filter
        .borrow_mut()
        .encode_render(&input_ref, &output, &mut encoder);
    queue_wgpu.submit([encoder.finish()]);
    frame.present();
    if let Err(error) = result {
        panic!("AppliedFilter render failed: {error}");
    }
    state.busy.set(false);
}
