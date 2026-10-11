//! GPU content hosting: `GpuContentView` renders through wgpu into a
//! `SwapChainPanel`, and `FilteredView` captures the wrapped visual through
//! Composition into a shared GPU texture, runs the effect, and presents the
//! result into an overlaying `SwapChainPanel`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use executor_core::{LocalExecutor, Task};
use waterui_core::Environment;
use waterui_core::layout::{ProposalSize, ViewDimensions};
use waterui_graphics::draw::kurbo;
use waterui_graphics::filter_view::ErasedEffect;
use waterui_graphics::filtrate::{
    EffectContext, EffectFrameClock, EffectInput, EffectOutput, ShapeTextures,
};
use waterui_graphics::{
    Context as GpuContext, FilteredView, Frame as GpuFrame, GpuContent, GpuContentView, GpuRuntime,
    RedrawHandle, SurfaceInputEvent, SurfacePointerButton, wgpu,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device5,
    ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_CPU_PAGE_PROPERTY, D3D12_FENCE_FLAG_SHARED, D3D12_HEAP_FLAG_SHARED,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_MEMORY_POOL, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS, D3D12_RESOURCE_STATE_COMMON,
    D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12Device, ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4,
};
use windows::core::{Interface as _, PCWSTR};
use windows_core::Interface;
use windows_numerics::{Vector2, Vector3};

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
    /// The last constraints and dimensions returned to XAML.
    measured: RefCell<Option<(ProposalSize, ViewDimensions)>>,
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
        measured: RefCell::new(None),
        size: Cell::new((0, 0)),
        scale: Cell::new(1.0),
        frame_pending: Cell::new(false),
        buttons: Cell::new(0),
        last_frame: Cell::new(None),
        start: Instant::now(),
    });

    let host_panel = crate::component::measured_host(&element, {
        let state = state.clone();
        move |proposal| {
            let answer = state.content.borrow().measure(proposal);
            *state.measured.borrow_mut() = Some((proposal, answer.clone()));
            answer
        }
    })
    .expect("MeasuredHost::compose");
    let host: UIElement = host_panel.cast().expect("MeasuredHost is a UIElement");
    let weak_host = host.downgrade().expect("MeasuredHost::downgrade");

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
                    let previous = state
                        .measured
                        .borrow()
                        .as_ref()
                        .map(|(proposal, dimensions)| (*proposal, dimensions.clone()));
                    if let Some((proposal, previous)) = previous {
                        let changed = state.content.borrow().measure(proposal) != previous;
                        if changed && let Some(host) = weak_host.upgrade() {
                            host.InvalidateMeasure()
                                .expect("UIElement::InvalidateMeasure");
                        }
                    }
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

    host
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
    /// The scale factors last written to the panel's `ScaleTransform`, so
    /// the transform is only re-set when the output or panel size changes.
    applied_scale: Cell<Option<(f64, f64)>>,
    interop: RefCell<Option<FilterInterop>>,
}

/// `Native<FilteredView>`: capture the subtree each frame, run the effect,
/// and present into an overlaying `SwapChainPanel`.
///
/// A filtered subtree that is itself a `FilteredView` renders into its own
/// overlay first — the outer Composition capture then includes the already
/// filtered pixels, so effect chains need no special handling.
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
    let holder = Grid::new().expect("Grid::new");
    crate::util::children(&grid)
        .Append(&holder)
        .expect("Grid::Children::Append");
    crate::util::children(&holder)
        .Append(&content)
        .expect("Grid::Children::Append");
    // Hide the unfiltered subtree on the holder's composition visual, not
    // through XAML opacity: a XAML `Opacity = 0` element's subtree is culled
    // from the render walk, which would leave the capture empty. Visual
    // opacity keeps the subtree drawing — the redirect capture sources the
    // content visual, unaffected by an ancestor's opacity — and hit testing
    // does not depend on it.
    ElementCompositionPreview::GetElementVisual(&holder)
        .expect("ElementCompositionPreview::GetElementVisual")
        .SetOpacity(0.0)
        .expect("IVisual::SetOpacity");
    let visual = ElementCompositionPreview::GetElementVisual(&content)
        .expect("ElementCompositionPreview::GetElementVisual");

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
        applied_scale: Cell::new(None),
        interop: RefCell::new(None),
    });

    let executor = renderer.executor().clone();

    {
        let state = state.clone();
        let runtime = runtime.clone();
        let wgpu_surface = wgpu_surface.clone();
        let content = content.clone();
        let visual = visual.clone();
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
                    visual.clone(),
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
// Flat capture/present pump.
async fn filter_frame(
    content: UIElement,
    visual: Visual,
    panel: UIElement,
    state: Rc<FilterState>,
    runtime: GpuRuntime,
    wgpu_surface: Rc<wgpu::Surface<'static>>,
) {
    // A `Rendering` tick can land before the first arrange; an element with
    // no rendered extent captures an empty visual, and wgpu rejects a
    // zero-dimension texture outright.
    let content_fe = framework(&content);
    let actual_width = content_fe.ActualWidth().expect("ActualWidth");
    let actual_height = content_fe.ActualHeight().expect("ActualHeight");
    if actual_width <= 0.0 || actual_height <= 0.0 {
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
            input_format: wgpu::TextureFormat::Rgba16Float,
            output_format: wgpu::TextureFormat::Rgba16Float,
        };
        let setup_result = state.effect.borrow_mut().setup(&ctx).await;
        setup_result.expect("FilteredView effect setup failed");
        state.setup_done.set(true);
    }

    let shared = runtime.context();
    let device = shared.device();
    let queue_wgpu = shared.queue();
    if state.interop.borrow().is_none() {
        let interop = FilterInterop::new(device, shared.generation(), &visual);
        *state.interop.borrow_mut() = Some(interop);
    }

    let scale = rasterization_scale(&content);
    let pw = (actual_width * scale).round() as u32;
    let ph = (actual_height * scale).round() as u32;
    if pw == 0 || ph == 0 {
        state.busy.set(false);
        return;
    }
    let (ow, oh) = state.effect.borrow().output_size(pw, ph);

    // The capture root's `Scale` is what enlarges the DIP-space render into
    // rasterization-scale pixels: `CaptureAsync`'s `size` is device pixels,
    // and a bare content visual would render in DIPs and fill only
    // 1/`scale` of the surface.
    let (capture_device, capture_root) = {
        let interop_state = state.interop.borrow();
        let interop = interop_state
            .as_ref()
            .expect("FilteredView interop state initialized");
        interop
            .capture_root
            .SetSize(Vector2 {
                x: actual_width as f32,
                y: actual_height as f32,
            })
            .expect("Visual::SetSize on capture root");
        interop
            .capture_root
            .SetScale(Vector3 {
                x: scale as f32,
                y: scale as f32,
                z: 1.0,
            })
            .expect("Visual::SetScale on capture root");
        (interop.capture_device.clone(), interop.capture_root.clone())
    };
    let capture_surface = capture_device
        .CaptureAsync(
            &capture_root,
            SizeInt32 {
                width: i32::try_from(pw).expect("capture width exceeds SizeInt32"),
                height: i32::try_from(ph).expect("capture height exceeds SizeInt32"),
            },
            DirectXPixelFormat::R16G16B16A16Float,
            DirectXAlphaMode::Premultiplied,
            1.0,
        )
        .expect("ICompositionGraphicsDevice4::CaptureAsync")
        .await
        .expect("Composition capture failed");
    let capture_surface_interop = capture_surface
        .cast::<ICompositionDrawingSurfaceInterop2>()
        .expect("captured Composition surface does not expose ICompositionDrawingSurfaceInterop2");

    let (capture_ready_value, d3d12_release_value) = {
        let mut interop_state = state.interop.borrow_mut();
        let interop = interop_state
            .as_mut()
            .expect("FilteredView interop state initialized");
        interop.ensure_size(device, (pw, ph), (ow, oh));
        interop.copy_capture(&capture_surface_interop)
    };

    // The effect declares the output size for the captured input — never the
    // grid: a mismatched swapchain makes `encode_render` fail `SizeMismatch`.
    if state.size.get() != (ow, oh) {
        state.size.set((ow, oh));
        configure_filter_surface(&wgpu_surface, device, ow, oh);
    }
    // A `SwapChainPanel` shows swapchain pixels 1:1 per DIP and this pin's
    // wgpu-hal sets no matrix transform, so an `OutputSize::Scale`/`Fixed`
    // effect would draw at the wrong size — the transform scales the output
    // back over the panel's layout bounds.
    let panel_fe = framework(&panel);
    let applied = (
        panel_fe.ActualWidth().expect("ActualWidth") / f64::from(ow),
        panel_fe.ActualHeight().expect("ActualHeight") / f64::from(oh),
    );
    if state.applied_scale.get() != Some(applied) {
        state.applied_scale.set(Some(applied));
        let transform = ScaleTransform::new().expect("ScaleTransform::new");
        transform
            .SetScaleX(applied.0)
            .expect("ScaleTransform::SetScaleX");
        transform
            .SetScaleY(applied.1)
            .expect("ScaleTransform::SetScaleY");
        panel
            .SetRenderTransform(
                &transform
                    .cast::<Transform>()
                    .expect("ScaleTransform is a Transform"),
            )
            .expect("UIElement::SetRenderTransform");
    }

    let frame = match wgpu_surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(frame)
        | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            configure_filter_surface(&wgpu_surface, device, ow, oh);
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
    let mut interop_state = state.interop.borrow_mut();
    let interop = interop_state
        .as_mut()
        .expect("FilteredView interop state initialized");
    let targets = interop
        .targets
        .as_ref()
        .expect("FilteredView targets initialized");
    let input_view = targets
        .input
        .create_view(&wgpu::TextureViewDescriptor::default());
    let effect_output_view = targets
        .output
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("waterui-winui filter encoder"),
    });
    // scRGB capture → premultiplied linear Display P3 effect input.
    encode_color_conversion(
        &mut encoder,
        &input_view,
        &interop.conversion.pipeline,
        &targets.capture_to_input,
    );
    let effect_input = EffectInput {
        device,
        queue: queue_wgpu,
        texture: &targets.input,
        view: input_view,
        format: wgpu::TextureFormat::Rgba16Float,
        width: pw,
        height: ph,
        timing: state.clock.borrow_mut().tick(),
        shape: ShapeTextures::default(),
    };
    let effect_output = EffectOutput {
        device,
        queue: queue_wgpu,
        texture: &targets.output,
        view: effect_output_view,
        format: wgpu::TextureFormat::Rgba16Float,
        width: ow,
        height: oh,
    };
    if let Err(error) =
        state
            .effect
            .borrow_mut()
            .encode_render(&effect_input, &effect_output, &mut encoder)
    {
        panic!("FilteredView effect render failed: {error}");
    }
    // linear Display P3 effect output → scRGB onto the surface texture.
    encode_color_conversion(
        &mut encoder,
        &output_view,
        &interop.conversion.pipeline,
        &targets.output_to_present,
    );

    let queue_guard = unsafe { queue_wgpu.as_hal::<wgpu::hal::api::Dx12>() }
        .expect("FilteredView requires the wgpu D3D12 backend");
    unsafe {
        queue_guard
            .as_raw()
            .Wait(&interop.d3d12_fence, capture_ready_value)
            .expect("D3D12 queue wait for Composition capture failed");
    }
    drop(queue_guard);
    queue_wgpu.submit([encoder.finish()]);
    let queue_guard = unsafe { queue_wgpu.as_hal::<wgpu::hal::api::Dx12>() }
        .expect("FilteredView requires the wgpu D3D12 backend");
    unsafe {
        queue_guard
            .as_raw()
            .Signal(&interop.d3d12_fence, d3d12_release_value)
            .expect("D3D12 queue signal after FilteredView failed");
    }
    drop(queue_guard);
    interop.last_d3d12_release = d3d12_release_value;
    queue_wgpu.present(frame);
    state.busy.set(false);
}

const SRGB_TO_P3: [[f32; 3]; 3] = [
    [0.822_461_96, 0.177_538_04, 0.0],
    [0.033_194_2, 0.966_805_8, 0.0],
    [0.017_082_632, 0.072_397_44, 0.910_519_96],
];

const P3_TO_SRGB: [[f32; 3]; 3] = [
    [1.224_940_1, -0.224_940_4, 0.0],
    [-0.042_056_9, 1.042_057_1, 0.0],
    [-0.019_637_6, -0.078_636_1, 1.098_273_5],
];

const FILTER_TARGET_USAGES: wgpu::TextureUsages = wgpu::TextureUsages::RENDER_ATTACHMENT
    .union(wgpu::TextureUsages::COPY_SRC)
    .union(wgpu::TextureUsages::COPY_DST)
    .union(wgpu::TextureUsages::TEXTURE_BINDING);

const COLOR_CONVERSION_SHADER: &str = include_str!("shaders/color_conversion.wgsl");

/// The scRGB ↔ premultiplied linear Display P3 conversion: one shader and
/// pipeline shared by both directions; only the primaries matrix in the
/// bound uniform differs.
struct ColorConversion {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    srgb_to_p3: wgpu::Buffer,
    p3_to_srgb: wgpu::Buffer,
}

impl ColorConversion {
    fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("waterui-winui filter color-conversion layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("waterui-winui filter color-conversion pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("waterui-winui filter color-conversion shader"),
            source: wgpu::ShaderSource::Wgsl(COLOR_CONVERSION_SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("waterui-winui filter color-conversion pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba16Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_group_layout,
            srgb_to_p3: uniform_buffer(device, SRGB_TO_P3, "sRGB to Display P3"),
            p3_to_srgb: uniform_buffer(device, P3_TO_SRGB, "Display P3 to sRGB"),
        }
    }

    fn bind_group(
        &self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        uniform: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("waterui-winui filter color-conversion bindings"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
            ],
        })
    }
}

/// A primaries matrix packed as three padded columns, matching the
/// `ConversionUniform` layout in `color_conversion.wgsl`.
fn uniform_buffer(device: &wgpu::Device, matrix: [[f32; 3]; 3], label: &str) -> wgpu::Buffer {
    let columns = [
        [matrix[0][0], matrix[1][0], matrix[2][0], 0.0],
        [matrix[0][1], matrix[1][1], matrix[2][1], 0.0],
        [matrix[0][2], matrix[1][2], matrix[2][2], 0.0],
    ];
    let uniform_bytes: Vec<u8> = columns
        .into_iter()
        .flatten()
        .flat_map(f32::to_ne_bytes)
        .collect();
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: u64::try_from(uniform_bytes.len()).expect("color-conversion uniform size"),
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: true,
    });
    uniform
        .get_mapped_range_mut(..)
        .expect("mapped color-conversion uniform range")
        .copy_from_slice(&uniform_bytes);
    uniform.unmap();
    uniform
}

struct FilterTargets {
    capture_texture: wgpu::Texture,
    capture_texture_11: ID3D11Texture2D,
    input: wgpu::Texture,
    output: wgpu::Texture,
    /// `capture_texture` → `input` bindings (scRGB → linear Display P3).
    capture_to_input: wgpu::BindGroup,
    /// `output` → surface bindings (linear Display P3 → scRGB). A bind group
    /// outlives the frame's swapchain texture only when the output texture is
    /// the bound source — it is — so this is built with `output`, not per
    /// frame.
    output_to_present: wgpu::BindGroup,
}

struct FilterInterop {
    /// The `SharedGpuContext` generation every member was built under. #104
    /// (device replacement for filtered views) rebuilds, on a generation
    /// change: the D3D11 device and its immediate context, both fences (the
    /// shared D3D12 fence and the D3D11 handle opened on it), the
    /// Composition graphics device, the shared D3D12/D3D11 capture textures
    /// in `targets`, and the colour-conversion pipelines.
    #[expect(
        dead_code,
        reason = "read by #104's device-replacement check, which does not exist yet"
    )]
    generation: u64,
    d3d12_device: ID3D12Device,
    d3d11_device: ID3D11Device5,
    d3d11_context: ID3D11DeviceContext4,
    capture_device: ICompositionGraphicsDevice4,
    /// Detached capture root: a `ContainerVisual` wrapping a redirect of the
    /// content visual, sized in DIPs and scaled to rasterization pixels.
    capture_root: Visual,
    d3d12_fence: ID3D12Fence,
    d3d11_fence: ID3D11Fence,
    next_capture_value: u64,
    last_d3d12_release: u64,
    targets: Option<FilterTargets>,
    conversion: ColorConversion,
}

/// Builds the detached capture root for `CaptureAsync`: a `ContainerVisual`
/// whose child redirects the content visual. `Size`/`Scale` are set per
/// frame from the element's `ActualSize` and the rasterization scale.
fn build_capture_root(compositor: &Compositor, visual: &Visual) -> Visual {
    let root = compositor
        .CreateContainerVisual()
        .expect("Compositor::CreateContainerVisual");
    let redirect = compositor
        .cast::<ICompositor6>()
        .expect("Compositor does not expose ICompositor6")
        .CreateRedirectVisual()
        .expect("Compositor::CreateRedirectVisual");
    redirect
        .SetSource(visual)
        .expect("IRedirectVisual::SetSource");
    root.Children()
        .expect("IContainerVisual::Children")
        .InsertAtTop(
            &redirect
                .cast::<Visual>()
                .expect("RedirectVisual is a Visual"),
        )
        .expect("VisualCollection::InsertAtTop");
    root.cast().expect("ContainerVisual is a Visual")
}

impl FilterInterop {
    #[allow(clippy::too_many_lines)] // linear interop wiring: each line is one device/fence step
    fn new(device: &wgpu::Device, generation: u64, visual: &Visual) -> Self {
        let (d3d12_device, adapter_luid) = {
            let device_guard = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
                .expect("FilteredView requires the wgpu D3D12 backend");
            (device_guard.raw_device().clone(), unsafe {
                device_guard.raw_device().GetAdapterLuid()
            })
        };
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .expect("CreateDXGIFactory2 failed");
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapterByLuid(adapter_luid) }
            .expect("DXGI adapter matching the wgpu device was not found");
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut d3d11_device = None;
        let mut d3d11_context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&raw mut d3d11_device),
                None,
                Some(&raw mut d3d11_context),
            )
        }
        .expect("D3D11CreateDevice on the wgpu adapter failed");
        let d3d11_device = d3d11_device
            .expect("D3D11CreateDevice returned no device")
            .cast::<ID3D11Device5>()
            .expect("D3D11 device does not expose ID3D11Device5");
        let d3d11_context = d3d11_context
            .expect("D3D11CreateDevice returned no immediate context")
            .cast::<ID3D11DeviceContext4>()
            .expect("D3D11 immediate context does not expose ID3D11DeviceContext4");
        let multithread: ID3D11Multithread = d3d11_context
            .cast()
            .expect("D3D11 immediate context does not expose ID3D11Multithread");
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }

        let composition_object: ICompositionObject = visual
            .cast()
            .expect("Visual does not expose ICompositionObject");
        let compositor = composition_object
            .Compositor()
            .expect("ICompositionObject::Compositor");
        let compositor_interop: ICompositorInterop = compositor
            .cast()
            .expect("Compositor does not expose ICompositorInterop");
        let mut graphics_device_raw = std::ptr::null_mut();
        unsafe {
            compositor_interop
                .CreateGraphicsDevice(
                    windows::core::Interface::as_raw(&d3d11_device),
                    &raw mut graphics_device_raw,
                )
                .ok()
                .expect("ICompositorInterop::CreateGraphicsDevice failed");
        }
        assert!(
            !graphics_device_raw.is_null(),
            "ICompositorInterop::CreateGraphicsDevice returned null"
        );
        let graphics_device: CompositionGraphicsDevice =
            unsafe { Interface::from_raw(graphics_device_raw) };
        let capture_device: ICompositionGraphicsDevice4 = graphics_device
            .cast()
            .expect("CompositionGraphicsDevice does not expose ICompositionGraphicsDevice4");
        let capture_root = build_capture_root(&compositor, visual);

        let d3d12_fence = unsafe {
            d3d12_device
                .CreateFence(0, D3D12_FENCE_FLAG_SHARED)
                .expect("ID3D12Device::CreateFence failed")
        };
        let fence_handle = unsafe {
            d3d12_device
                .CreateSharedHandle(&d3d12_fence, None, GENERIC_ALL.0, PCWSTR::null())
                .expect("ID3D12Device::CreateSharedHandle for fence failed")
        };
        let mut d3d11_fence = None;
        let d3d11_fence_result = unsafe {
            d3d11_device.OpenSharedFence::<ID3D11Fence>(fence_handle, &raw mut d3d11_fence)
        };
        unsafe { CloseHandle(fence_handle) }.expect("CloseHandle for shared fence failed");
        d3d11_fence_result.expect("ID3D11Device5::OpenSharedFence failed");
        let d3d11_fence = d3d11_fence.expect("OpenSharedFence returned no D3D11 fence");

        Self {
            generation,
            d3d12_device,
            d3d11_device,
            d3d11_context,
            capture_device,
            capture_root,
            d3d12_fence,
            d3d11_fence,
            next_capture_value: 1,
            last_d3d12_release: 0,
            targets: None,
            conversion: ColorConversion::new(device),
        }
    }

    fn ensure_size(
        &mut self,
        device: &wgpu::Device,
        capture_size: (u32, u32),
        output_size: (u32, u32),
    ) {
        let capture_size_changed = self.targets.as_ref().is_none_or(|targets| {
            (
                targets.capture_texture.width(),
                targets.capture_texture.height(),
            ) != capture_size
        });
        let output_size_changed = self
            .targets
            .as_ref()
            .is_none_or(|targets| (targets.output.width(), targets.output.height()) != output_size);
        if !capture_size_changed && !output_size_changed {
            return;
        }

        let new_capture = if capture_size_changed {
            let (width, height) = capture_size;
            let (capture_texture, capture_texture_11) =
                self.create_capture_target(device, width, height);
            let input = create_filter_target(device, width, height, "waterui-winui filter input");
            let capture_view = capture_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let capture_to_input =
                self.conversion
                    .bind_group(device, &capture_view, &self.conversion.srgb_to_p3);
            Some((capture_texture, capture_texture_11, input, capture_to_input))
        } else {
            None
        };
        let new_output = output_size_changed.then(|| {
            let output = create_filter_target(
                device,
                output_size.0,
                output_size.1,
                "waterui-winui filter output",
            );
            let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
            let output_to_present =
                self.conversion
                    .bind_group(device, &output_view, &self.conversion.p3_to_srgb);
            (output, output_to_present)
        });

        if let Some(targets) = self.targets.as_mut() {
            if let Some((capture_texture, capture_texture_11, input, capture_to_input)) =
                new_capture
            {
                targets.capture_texture = capture_texture;
                targets.capture_texture_11 = capture_texture_11;
                targets.input = input;
                targets.capture_to_input = capture_to_input;
            }
            if let Some((output, output_to_present)) = new_output {
                targets.output = output;
                targets.output_to_present = output_to_present;
            }
        } else {
            let (capture_texture, capture_texture_11, input, capture_to_input) =
                new_capture.expect("initial FilteredView capture targets were created");
            let (output, output_to_present) =
                new_output.expect("initial FilteredView output target was created");
            self.targets = Some(FilterTargets {
                capture_texture,
                capture_texture_11,
                input,
                output,
                capture_to_input,
                output_to_present,
            });
        }
    }

    fn create_capture_target(
        &self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> (wgpu::Texture, ID3D11Texture2D) {
        let (resource, capture_texture_11) =
            create_shared_capture_texture(&self.d3d12_device, &self.d3d11_device, width, height);
        let hal_texture = {
            let device_guard = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
                .expect("FilteredView requires the wgpu D3D12 backend");
            let hal_texture = unsafe {
                wgpu::hal::dx12::Device::texture_from_raw(
                    resource,
                    wgpu::TextureFormat::Rgba16Float,
                    wgpu::TextureDimension::D2,
                    wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    1,
                    1,
                )
            };
            drop(device_guard);
            hal_texture
        };
        let capture_texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("waterui-winui Composition capture texture"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba16Float,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        };
        (capture_texture, capture_texture_11)
    }

    fn copy_capture(&mut self, surface: &ICompositionDrawingSurfaceInterop2) -> (u64, u64) {
        let targets = self
            .targets
            .as_ref()
            .expect("capture targets initialized before CopySurface");
        unsafe {
            self.d3d11_context
                .Wait(&self.d3d11_fence, self.last_d3d12_release)
                .expect("D3D11 wait for prior wgpu read failed");
            surface
                .CopySurface(
                    windows::core::Interface::as_raw(&targets.capture_texture_11),
                    0,
                    0,
                    std::ptr::null(),
                )
                .ok()
                .expect("ICompositionDrawingSurfaceInterop2::CopySurface failed");
            let capture_ready_value = self.next_capture_value;
            let d3d12_release_value = capture_ready_value
                .checked_add(1)
                .expect("FilteredView fence value overflow");
            self.d3d11_context
                .Signal(&self.d3d11_fence, capture_ready_value)
                .expect("D3D11 signal after Composition capture failed");
            self.d3d11_context.Flush();
            self.next_capture_value = capture_ready_value
                .checked_add(2)
                .expect("FilteredView fence value overflow");
            (capture_ready_value, d3d12_release_value)
        }
    }
}

fn create_shared_capture_texture(
    d3d12_device: &ID3D12Device,
    d3d11_device: &ID3D11Device5,
    width: u32,
    height: u32,
) -> (ID3D12Resource, ID3D11Texture2D) {
    let resource_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
            | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
    };
    let heap_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY::default(),
        MemoryPoolPreference: D3D12_MEMORY_POOL::default(),
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut resource = None;
    unsafe {
        d3d12_device.CreateCommittedResource(
            &raw const heap_properties,
            D3D12_HEAP_FLAG_SHARED,
            &raw const resource_desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &raw mut resource,
        )
    }
    .expect("ID3D12Device::CreateCommittedResource for capture failed");
    let resource: ID3D12Resource =
        resource.expect("CreateCommittedResource returned no capture resource");
    let texture_handle = unsafe {
        d3d12_device
            .CreateSharedHandle(&resource, None, GENERIC_ALL.0, PCWSTR::null())
            .expect("ID3D12Device::CreateSharedHandle for capture failed")
    };
    let capture_texture_11_result =
        unsafe { d3d11_device.OpenSharedResource1::<ID3D11Texture2D>(texture_handle) };
    unsafe { CloseHandle(texture_handle) }.expect("CloseHandle for shared capture texture failed");
    let capture_texture_11 =
        capture_texture_11_result.expect("ID3D11Device1::OpenSharedResource1 failed");
    (resource, capture_texture_11)
}

fn create_filter_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: FILTER_TARGET_USAGES,
        view_formats: &[],
    })
}

fn configure_filter_surface(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    width: u32,
    height: u32,
) {
    surface.configure(
        device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Rgba16Float,
            color_space: wgpu::SurfaceColorSpace::ExtendedSrgbLinear,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            view_formats: vec![],
        },
    );
}

fn encode_color_conversion(
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("waterui-winui filter color-conversion pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.draw(0..3, 0..1);
}

#[cfg(test)]
mod color_conversion_tests {
    use super::{P3_TO_SRGB, SRGB_TO_P3};

    fn multiply(left: [[f32; 3]; 3], right: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
        std::array::from_fn(|row| {
            std::array::from_fn(|column| {
                (0..3)
                    .map(|index| left[row][index] * right[index][column])
                    .sum()
            })
        })
    }

    fn transform(matrix: [[f32; 3]; 3], rgb: [f32; 3]) -> [f32; 3] {
        std::array::from_fn(|row| (0..3).map(|column| matrix[row][column] * rgb[column]).sum())
    }

    #[test]
    fn display_p3_conversion_matrices_are_inverse_and_preserve_white() {
        let product = multiply(SRGB_TO_P3, P3_TO_SRGB);
        for (row, values) in product.into_iter().enumerate() {
            for (column, value) in values.into_iter().enumerate() {
                let expected = if row == column { 1.0 } else { 0.0 };
                assert!((value - expected).abs() < 1e-5);
            }
        }
        for matrix in [SRGB_TO_P3, P3_TO_SRGB] {
            for component in transform(matrix, [1.0, 1.0, 1.0]) {
                assert!((component - 1.0).abs() < 1e-5);
            }
        }
    }
}
