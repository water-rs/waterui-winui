//! GPU content hosting: `GpuContentView` renders through wgpu into a
//! `SwapChainPanel`, and `FilteredView` captures the wrapped element with
//! `RenderTargetBitmap`, runs the effect on the GPU, and presents the result
//! into an overlaying `SwapChainPanel`.
//!
//! The capture path reads the filtered subtree back through the compositor
//! once per frame; the direct `GpuContentView` path presents without a
//! readback.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use executor_core::{LocalExecutor, Task};
use waterui_core::Environment;
use waterui_graphics::draw::kurbo;
use waterui_graphics::filter_view::ErasedEffect;
use waterui_graphics::filtrate::{
    EffectContext, EffectFrameClock, EffectInput, EffectOutput, ShapeTextures,
};
use waterui_graphics::{
    Context as GpuContext, FilteredView, Frame as GpuFrame, GpuContent, GpuContentView, GpuRuntime,
    RedrawHandle, SurfaceInputEvent, SurfacePointerButton, wgpu,
};
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::renderer::WinUiRenderer;
use crate::util::framework;

/// Shared per-surface state mutated from event handlers and the render pump.
struct SurfaceState {
    /// The view: its per-frame UI hook and input handler live on it while
    /// its content draws.
    view: GpuContentView,
    /// The `GpuContent` the view installed on this surface.
    content: RefCell<Box<dyn GpuContent>>,
    /// Physical pixel size of the swapchain.
    size: Cell<(u32, u32)>,
    /// Pixels per logical point, from the last `SizeChanged`.
    scale: Cell<f32>,
    /// Whether a frame is already queued.
    frame_pending: Cell<bool>,
    /// Buttons currently held, one bit per `SurfacePointerButton` —
    /// `PointerCanceled`/`PointerCaptureLost` synthesize releases from it.
    buttons: Cell<u32>,
    /// Clock feeding `Frame::elapsed`/`delta`.
    last_frame: Cell<Option<Instant>>,
    start: Instant,
}

/// Renders a `GpuContentView` into a `SwapChainPanel`.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
// Flat wiring; pixel conversions saturate intentionally via `as` after
// `.max(1.0)`.
pub(crate) fn render_gpu_content(
    renderer: &WinUiRenderer,
    mut view: GpuContentView,
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
                .instance()
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::SwapChainPanel(native.as_raw()))
        }
        .expect("wgpu surface creation from SwapChainPanel failed"),
    );

    let context = runtime.context();
    let format = pick_surface_format(
        &wgpu_surface,
        context.adapter(),
        view.resolved_hdr_preference(),
    );
    let gpu_content = view.take_content();

    let state = Rc::new(SurfaceState {
        view,
        content: RefCell::new(gpu_content),
        size: Cell::new((0, 0)),
        scale: Cell::new(1.0),
        frame_pending: Cell::new(false),
        buttons: Cell::new(0),
        last_frame: Cell::new(None),
        start: Instant::now(),
    });

    // `GpuContent::setup` is synchronous and runs on the dispatcher thread.
    // The `Context::redraw` handle it stashes may fire from any thread — a
    // decoder, a signal watcher — so it captures only the channel `Sender`:
    // a `try_send` on the `bounded(1)` channel coalesces repeated requests
    // into one queued pump. A spawned task on the dispatcher owns a `Weak`
    // back into the state — when the view drops, its content drops the
    // `Sender`, `recv` ends the loop, and nothing upgrades: a handle
    // outliving its view redraws nothing, with no reference cycle.
    {
        let (sender, receiver) = async_channel::bounded::<()>(1);
        let redraw = RedrawHandle::new(move || {
            let _ = sender.try_send(());
        });
        let weak = Rc::downgrade(&state);
        let surface = wgpu_surface.clone();
        let runtime = runtime.clone();
        renderer
            .executor()
            .spawn_local(async move {
                while receiver.recv().await.is_ok() {
                    let Some(state) = weak.upgrade() else {
                        break;
                    };
                    pump_frame(&surface, &runtime, &state, format);
                }
            })
            .detach();
        // `context()` returns an owned `Arc`: the runtime may swap in a
        // rebuilt context after device loss, so each frame of work pins
        // one generation by holding it in a local.
        state.content.borrow_mut().setup(&GpuContext {
            adapter: context.adapter(),
            device: context.device(),
            queue: context.queue(),
            format,
            redraw,
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
                state.scale.set(scale as f32);
                if state.size.get() != (w, h) {
                    state.size.set((w, h));
                    configure(&wgpu_surface, &runtime, format, w, h);
                }
                pump_frame(&wgpu_surface, &runtime, &state, format);
            })
            .expect("FrameworkElement::SizeChanged");
        crate::util::store_event_revoker(&fe, revoker);
    }

    // Pointer events route to the view's input handler, if the content
    // declared one. Positions are logical, surface-local points. XAML
    // reports extra presses and releases while a pointer is held as
    // `PointerMoved` with a `PointerUpdateKind`, so all three events share
    // `pointer_input`; `PointerCanceled`/`PointerCaptureLost` end a held
    // sequence without transitions, so they synthesize the missing
    // releases from the tracked buttons.
    {
        let state = state.clone();
        let revoker = element
            .PointerMoved(move |sender, args| {
                pointer_input(&state, sender, args);
            })
            .expect("UIElement::PointerMoved");
        crate::util::store_event_revoker(&fe, revoker);
    }
    {
        let state = state.clone();
        let revoker = element
            .PointerPressed(move |sender, args| {
                pointer_input(&state, sender, args);
            })
            .expect("UIElement::PointerPressed");
        crate::util::store_event_revoker(&fe, revoker);
    }
    {
        let state = state.clone();
        let revoker = element
            .PointerReleased(move |sender, args| {
                pointer_input(&state, sender, args);
            })
            .expect("UIElement::PointerReleased");
        crate::util::store_event_revoker(&fe, revoker);
    }
    {
        let state = state.clone();
        let revoker = element
            .PointerCanceled(move |sender, args| {
                release_held_buttons(&state, sender, args);
            })
            .expect("UIElement::PointerCanceled");
        crate::util::store_event_revoker(&fe, revoker);
    }
    {
        let state = state.clone();
        let revoker = element
            .PointerCaptureLost(move |sender, args| {
                release_held_buttons(&state, sender, args);
            })
            .expect("UIElement::PointerCaptureLost");
        crate::util::store_event_revoker(&fe, revoker);
    }

    element
}

/// One bit per `SurfacePointerButton`, in declaration order — the tracked
/// set of buttons currently held on a surface.
const BUTTONS: [SurfacePointerButton; 5] = [
    SurfacePointerButton::Primary,
    SurfacePointerButton::Secondary,
    SurfacePointerButton::Middle,
    SurfacePointerButton::Back,
    SurfacePointerButton::Forward,
];

/// The bit `button` occupies in `SurfaceState::buttons`.
fn button_bit(button: SurfacePointerButton) -> u32 {
    BUTTONS
        .iter()
        .position(|held| *held == button)
        .map(|index| 1_u32 << index)
        .expect("every SurfacePointerButton has a bit")
}

/// Emits a release for every button still held: `PointerCanceled` and
/// `PointerCaptureLost` arrive in place of the releases, and the view must
/// see each held button go up exactly once.
fn release_held_buttons(
    state: &Rc<SurfaceState>,
    sender: windows_core::Ref<windows_core::IInspectable>,
    args: windows_core::Ref<PointerRoutedEventArgs>,
) {
    let held = state.buttons.replace(0);
    if held == 0 || !state.view.wants_input_events() {
        return;
    }
    let element: UIElement = sender
        .ok()
        .expect("pointer event sender")
        .cast()
        .expect("UIElement");
    let args = args.ok().expect("pointer event args");
    let position = args
        .GetCurrentPoint(&element)
        .expect("PointerRoutedEventArgs::GetCurrentPoint")
        .Position()
        .expect("PointerPoint::Position");
    let position = kurbo::Point::new(f64::from(position.x), f64::from(position.y));
    for button in BUTTONS {
        if held & button_bit(button) != 0 {
            state.view.input(&SurfaceInputEvent::PointerButton {
                pressed: false,
                button,
                position,
            });
        }
    }
}

/// Routes a `PointerMoved`/`PointerPressed`/`PointerReleased` event into the
/// view's input handler when the content declared one.
///
/// The `PointerUpdateKind` decides: a transition in the W3C button set
/// becomes `PointerButton` — a press captures the pointer so moves keep
/// arriving while it is held, and is marked `Handled` — and anything else
/// (including `Other`) is a plain `PointerMove`. Every event funnels here,
/// including the `PointerMoved`s XAML emits for extra buttons during a held
/// press.
fn pointer_input(
    state: &Rc<SurfaceState>,
    sender: windows_core::Ref<windows_core::IInspectable>,
    args: windows_core::Ref<PointerRoutedEventArgs>,
) {
    let element: UIElement = sender
        .ok()
        .expect("pointer event sender")
        .cast()
        .expect("UIElement");
    let args = args.ok().expect("pointer event args");
    let point = args
        .GetCurrentPoint(&element)
        .expect("PointerRoutedEventArgs::GetCurrentPoint");
    if !state.view.wants_input_events() {
        return;
    }
    let position = point.Position().expect("PointerPoint::Position");
    let position = kurbo::Point::new(f64::from(position.x), f64::from(position.y));
    let kind = point
        .Properties()
        .expect("PointerPoint::Properties")
        .PointerUpdateKind()
        .expect("PointerPointProperties::PointerUpdateKind");
    let transition = match kind {
        PointerUpdateKind::LeftButtonPressed => Some((SurfacePointerButton::Primary, true)),
        PointerUpdateKind::LeftButtonReleased => Some((SurfacePointerButton::Primary, false)),
        PointerUpdateKind::RightButtonPressed => Some((SurfacePointerButton::Secondary, true)),
        PointerUpdateKind::RightButtonReleased => Some((SurfacePointerButton::Secondary, false)),
        PointerUpdateKind::MiddleButtonPressed => Some((SurfacePointerButton::Middle, true)),
        PointerUpdateKind::MiddleButtonReleased => Some((SurfacePointerButton::Middle, false)),
        PointerUpdateKind::XButton1Pressed => Some((SurfacePointerButton::Back, true)),
        PointerUpdateKind::XButton1Released => Some((SurfacePointerButton::Back, false)),
        PointerUpdateKind::XButton2Pressed => Some((SurfacePointerButton::Forward, true)),
        PointerUpdateKind::XButton2Released => Some((SurfacePointerButton::Forward, false)),
        _ => None,
    };
    match transition {
        Some((button, pressed)) => {
            if pressed {
                element
                    .CapturePointer(&args.Pointer().expect("PointerRoutedEventArgs::Pointer"))
                    .expect("UIElement::CapturePointer");
                args.SetHandled(true)
                    .expect("PointerRoutedEventArgs::SetHandled");
                state.buttons.set(state.buttons.get() | button_bit(button));
            } else {
                state.buttons.set(state.buttons.get() & !button_bit(button));
            }
            state.view.input(&SurfaceInputEvent::PointerButton {
                pressed,
                button,
                position,
            });
        }
        None => state
            .view
            .input(&SurfaceInputEvent::PointerMove { position }),
    }
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
    surface: &Rc<wgpu::Surface<'static>>,
    runtime: &GpuRuntime,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) {
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
    };
    surface.configure(runtime.context().device(), &config);
}

/// Renders one frame when the surface is configured and no frame is
/// already pending.
fn pump_frame(
    surface: &Rc<wgpu::Surface<'static>>,
    runtime: &GpuRuntime,
    state: &Rc<SurfaceState>,
    format: wgpu::TextureFormat,
) {
    if state.frame_pending.get() {
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
    let target = texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    let now = Instant::now();
    let delta = state
        .last_frame
        .replace(Some(now))
        .map_or(Duration::ZERO, |last| now - last);
    let elapsed = now - state.start;

    // The UI-side frame hook runs on this thread before the content draws.
    state.view.frame();

    let context = runtime.context();
    let mut frame = GpuFrame::new(
        context.device(),
        context.queue(),
        &texture.texture,
        &target,
        format,
        (width, height),
        state.scale.get(),
        (elapsed, delta),
    );
    state.content.borrow_mut().render(&mut frame);
    let redraw = frame.redraw_requested();
    context.queue().present(texture);
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

/// Shared state for the `FilteredView` capture/present pump.
struct FilterState {
    effect: RefCell<Box<dyn ErasedEffect>>,
    clock: RefCell<EffectFrameClock>,
    busy: Cell<bool>,
    /// Whether `Effect::setup` compiled the pass graph.
    setup_done: Cell<bool>,
    /// Last physical size the output swapchain was configured with.
    size: Cell<(u32, u32)>,
}

/// `Native<FilteredView>`: capture the subtree each frame, run the effect,
/// and present into an overlaying `SwapChainPanel`.
///
/// A filtered subtree that is itself a `FilteredView` renders into its own
/// overlay first — the outer capture then reads the already-filtered pixels,
/// so effect chains need no special handling.
// Flat capture/present wiring.
pub(crate) fn render_filtered_view(
    renderer: &mut WinUiRenderer,
    filtered: FilteredView,
    env: &Environment,
) -> UIElement {
    let FilteredView {
        content,
        effect,
        guards,
    } = filtered;
    let content = renderer.render_any(content, env);
    // The subscriptions feeding the effect's reactive parameters live as
    // long as the realized element.
    crate::util::store_retained(&framework(&content), Box::new(guards));

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
                .instance()
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::SwapChainPanel(native.as_raw()))
        }
        .expect("wgpu surface creation from SwapChainPanel failed"),
    );

    let state = Rc::new(FilterState {
        effect: RefCell::new(effect.build()),
        clock: RefCell::new(EffectFrameClock::new()),
        busy: Cell::new(false),
        setup_done: Cell::new(false),
        size: Cell::new((0, 0)),
    });

    let executor = renderer.executor().clone();

    {
        let state = state.clone();
        let runtime = runtime.clone();
        let wgpu_surface = wgpu_surface.clone();
        let content = content.clone();
        let panel = panel_element.clone();
        // `CompositionTarget::Rendering` already fires on the UI thread once
        // per compositor frame, so no effect-side redraw callback is needed:
        // a dirty parameter simply renders on the next tick.
        let grid_for_revoker = grid.clone();
        let revoker = CompositionTarget::Rendering(move |_sender, _args| {
            if state.busy.replace(true) {
                return;
            }
            executor
                .spawn_local(filter_frame(
                    content.clone(),
                    panel.clone(),
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
#[expect(
    clippy::await_holding_refcell_ref,
    reason = "`ErasedEffect::setup` takes `&mut self`, so the borrow must live across the await; \
              `busy` admits one frame at a time and this fn is the only borrower of `effect`"
)]
// Flat capture/present pump; pixel conversions saturate intentionally.
async fn filter_frame(
    content: UIElement,
    panel: UIElement,
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

    // `encode_render` requires a compiled pass graph; `Effect::setup` builds
    // it asynchronously, so the first frame compiles before any capture work.
    if !state.setup_done.get() {
        let shared = runtime.context();
        let ctx = EffectContext {
            device: shared.device(),
            queue: shared.queue(),
            input_format: wgpu::TextureFormat::Bgra8Unorm,
            output_format: wgpu::TextureFormat::Bgra8Unorm,
        };
        let setup_result = state.effect.borrow_mut().setup(&ctx).await;
        setup_result.expect("FilteredView effect setup failed");
        state.setup_done.set(true);
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

    // The effect declares the output size for the captured input — never the
    // grid: a mismatched swapchain makes `encode_render` fail `SizeMismatch`.
    let (ow, oh) = state.effect.borrow().output_size(pw, ph);
    if state.size.get() != (ow, oh) {
        state.size.set((ow, oh));
        configure(
            &wgpu_surface,
            &runtime,
            wgpu::TextureFormat::Bgra8Unorm,
            ow,
            oh,
        );
    }
    // A `SwapChainPanel` shows swapchain pixels 1:1 per DIP and this pin's
    // wgpu-hal sets no matrix transform, so an `OutputSize::Scale`/`Fixed`
    // effect would draw at the wrong size — the transform scales the output
    // back over the panel's layout bounds.
    let panel_fe = framework(&panel);
    let scale = ScaleTransform::new().expect("ScaleTransform::new");
    scale
        .SetScaleX(panel_fe.ActualWidth().expect("ActualWidth") / f64::from(ow))
        .expect("ScaleTransform::SetScaleX");
    scale
        .SetScaleY(panel_fe.ActualHeight().expect("ActualHeight") / f64::from(oh))
        .expect("ScaleTransform::SetScaleY");
    panel
        .SetRenderTransform(
            &scale
                .cast::<Transform>()
                .expect("ScaleTransform is a Transform"),
        )
        .expect("UIElement::SetRenderTransform");

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

    let shared = runtime.context();
    let device = shared.device();
    let queue_wgpu = shared.queue();

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
                ow,
                oh,
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

    let input_ref = EffectInput {
        device,
        queue: queue_wgpu,
        texture: &input,
        view: input_view,
        format: wgpu::TextureFormat::Bgra8Unorm,
        width: pw,
        height: ph,
        timing: state.clock.borrow_mut().tick(),
        shape: ShapeTextures::default(),
    };
    let output = EffectOutput {
        device,
        queue: queue_wgpu,
        texture: &frame.texture,
        view: output_view,
        format: wgpu::TextureFormat::Bgra8Unorm,
        width: ow,
        height: oh,
    };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("waterui-winui filter encoder"),
    });
    let result = state
        .effect
        .borrow_mut()
        .encode_render(&input_ref, &output, &mut encoder);
    queue_wgpu.submit([encoder.finish()]);
    queue_wgpu.present(frame);
    if let Err(error) = result {
        panic!("FilteredView effect render failed: {error}");
    }
    state.busy.set(false);
}
