//! `WinUI` theme resources projected into `WaterUI` theme slots.
//!
//! `WaterUI` is theme-neutral: `text("…")` resolves the `Body` font token and
//! `Foreground`/`Background` color signals out of the [`Environment`], and a
//! backend that never installs them panics at render time. The values here are
//! read live out of the application's `WinUI` resource chain — the same
//! `ThemeResource`s XAML controls bind to — so user-supplied theme packs stay
//! authoritative.

use nami::{Binding, Signal, SignalExt, binding};
use waterui::theme::color::{
    Accent, AccentContainer, AccentForeground, Background, Border, Error, ErrorForeground,
    Foreground, MutedForeground, SelectionContainer, SelectionForeground, Surface, SurfaceVariant,
    Tertiary, TertiaryContainer,
};
use waterui::theme::{
    ColorScheme, install_color_scheme, install_color_signal, install_font_signal,
    installed_color_scheme, installed_color_signal,
};
use waterui_core::Environment;
use waterui_graphics::color::{ResolvedColor, Srgb};
use waterui_text::font::{
    Body, Caption, FontWeight, Footnote, Headline, ResolvedFont, Subheadline, Title,
};
use windows_collections::IMap;
use windows_core::{HSTRING, IInspectable, Interface};
use windows_reference::IReference;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::util::store_event_revoker;

type Resources = IMap<IInspectable, IInspectable>;

/// Reactive handles for the theme tokens this backend injected. Stored in the
/// environment so every window can attach a refresh hook; user-installed
/// `Theme` values are never overridden.
#[derive(Clone)]
struct ThemeSignals {
    background: Binding<ResolvedColor>,
    surface: Binding<ResolvedColor>,
    surface_variant: Binding<ResolvedColor>,
    border: Binding<ResolvedColor>,
    foreground: Binding<ResolvedColor>,
    muted_foreground: Binding<ResolvedColor>,
    accent: Binding<ResolvedColor>,
    accent_container: Binding<ResolvedColor>,
    accent_foreground: Binding<ResolvedColor>,
    tertiary: Binding<ResolvedColor>,
    tertiary_container: Binding<ResolvedColor>,
    selection_container: Binding<ResolvedColor>,
    selection_foreground: Binding<ResolvedColor>,
    error: Binding<ResolvedColor>,
    error_foreground: Binding<ResolvedColor>,
    color_scheme: Binding<ColorScheme>,
}

/// Installs the `WinUI` theme palette and typography ramp into `env`.
///
/// Runs once per application — a second call is a no-op so user-provided
/// `Theme` plugins always win. Must run on the UI thread after the
/// `Application` exists (i.e. inside `OnLaunched`).
pub fn install(env: &mut Environment) -> windows_core::Result<()> {
    if env.get::<ThemeSignals>().is_some() {
        return Ok(());
    }
    let resources = theme_resources()?;
    let signals = ThemeSignals::read(&resources);

    install_missing::<Background>(env, &signals.background);
    install_missing::<Surface>(env, &signals.surface);
    install_missing::<SurfaceVariant>(env, &signals.surface_variant);
    install_missing::<Border>(env, &signals.border);
    install_missing::<Foreground>(env, &signals.foreground);
    install_missing::<MutedForeground>(env, &signals.muted_foreground);
    install_missing::<Accent>(env, &signals.accent);
    install_missing::<AccentContainer>(env, &signals.accent_container);
    install_missing::<AccentForeground>(env, &signals.accent_foreground);
    install_missing::<Tertiary>(env, &signals.tertiary);
    install_missing::<TertiaryContainer>(env, &signals.tertiary_container);
    install_missing::<SelectionContainer>(env, &signals.selection_container);
    install_missing::<SelectionForeground>(env, &signals.selection_foreground);
    install_missing::<Error>(env, &signals.error);
    install_missing::<ErrorForeground>(env, &signals.error_foreground);

    if installed_color_scheme(env).is_none() {
        install_color_scheme(env, signals.color_scheme.computed());
    }

    install_font_missing::<Body>(env, font(&resources, "BodyTextBlockStyle"));
    install_font_missing::<Title>(env, font(&resources, "TitleTextBlockStyle"));
    install_font_missing::<Headline>(env, font(&resources, "SubtitleTextBlockStyle"));
    install_font_missing::<Subheadline>(env, font(&resources, "BodyStrongTextBlockStyle"));
    install_font_missing::<Caption>(env, font(&resources, "CaptionTextBlockStyle"));
    install_font_missing::<Footnote>(env, font(&resources, "CaptionTextBlockStyle"));

    env.insert(signals);
    Ok(())
}

/// Hooks `ActualThemeChanged` on a window's root element so the installed
/// signals re-read the resource chain whenever the OS theme flips.
pub fn attach(root: &FrameworkElement, env: &Environment) -> windows_core::Result<()> {
    let Some(signals) = env.get::<ThemeSignals>().cloned() else {
        return Ok(());
    };
    let update = signals.clone();
    let revoker = root.ActualThemeChanged(move |sender, _| {
        if let Ok(element) = sender.ok() {
            update.refresh(element);
        }
    })?;
    store_event_revoker(root, revoker);
    // Converge immediately: the initial `ColorScheme` read happens before any
    // element exists, so the first attach resolves the real `ActualTheme`.
    signals.refresh(root);
    Ok(())
}

fn install_missing<T: 'static>(env: &mut Environment, value: &Binding<ResolvedColor>) {
    if installed_color_signal::<T>(env).is_none() {
        install_color_signal::<T>(env, value.clone().computed());
    }
}

fn install_font_missing<T: 'static>(env: &mut Environment, font: ResolvedFont) {
    if env.query::<T, nami::Computed<ResolvedFont>>().is_none() {
        install_font_signal::<T>(env, nami::Computed::constant(font));
    }
}

impl ThemeSignals {
    /// Reads every color slot plus the color scheme out of the live resource
    /// chain for `root`'s current theme.
    fn read(resources: &Resources) -> Self {
        Self {
            background: binding(brush_color(resources, "SolidBackgroundFillColorBaseBrush")),
            surface: binding(brush_color(resources, "LayerFillColorDefaultBrush")),
            surface_variant: binding(brush_color(resources, "LayerFillColorAltBrush")),
            border: binding(brush_color(resources, "CardStrokeColorDefaultBrush")),
            foreground: binding(brush_color(resources, "TextFillColorPrimaryBrush")),
            muted_foreground: binding(brush_color(resources, "TextFillColorSecondaryBrush")),
            accent: binding(brush_color(resources, "AccentFillColorDefaultBrush")),
            accent_container: binding(brush_color(
                resources,
                "AccentAcrylicBackgroundFillColorDefaultBrush",
            )),
            accent_foreground: binding(brush_color(resources, "TextOnAccentFillColorPrimaryBrush")),
            tertiary: binding(brush_color(resources, "AccentFillColorDefaultBrush")),
            tertiary_container: binding(brush_color(
                resources,
                "AccentAcrylicBackgroundFillColorDefaultBrush",
            )),
            selection_container: binding(brush_color(resources, "AccentFillColorSecondaryBrush")),
            selection_foreground: binding(brush_color(
                resources,
                "TextOnAccentFillColorPrimaryBrush",
            )),
            error: binding(brush_color(resources, "SystemFillColorCriticalBrush")),
            error_foreground: binding(brush_color(resources, "TextOnAccentFillColorPrimaryBrush")),
            color_scheme: binding(ColorScheme::Light),
        }
    }

    /// Re-reads the palette and scheme after `root` reported a theme change.
    fn refresh(&self, root: &FrameworkElement) {
        let resources = theme_resources().expect("Application resources on theme change");
        let fresh = Self::read(&resources);
        self.background.set(fresh.background.snapshot());
        self.surface.set(fresh.surface.snapshot());
        self.surface_variant.set(fresh.surface_variant.snapshot());
        self.border.set(fresh.border.snapshot());
        self.foreground.set(fresh.foreground.snapshot());
        self.muted_foreground.set(fresh.muted_foreground.snapshot());
        self.accent.set(fresh.accent.snapshot());
        self.accent_container.set(fresh.accent_container.snapshot());
        self.accent_foreground
            .set(fresh.accent_foreground.snapshot());
        self.tertiary.set(fresh.tertiary.snapshot());
        self.tertiary_container
            .set(fresh.tertiary_container.snapshot());
        self.selection_container
            .set(fresh.selection_container.snapshot());
        self.selection_foreground
            .set(fresh.selection_foreground.snapshot());
        self.error.set(fresh.error.snapshot());
        self.error_foreground.set(fresh.error_foreground.snapshot());
        self.color_scheme.set(scheme_of(root));
    }
}

/// Maps `FrameworkElement::ActualTheme` to `WaterUI`'s `ColorScheme`.
fn scheme_of(element: &FrameworkElement) -> ColorScheme {
    if element
        .ActualTheme()
        .expect("FrameworkElement::ActualTheme")
        == ElementTheme::Dark
    {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    }
}

/// The application-level resource chain (merged dictionaries included) as a
/// lookup map.
fn theme_resources() -> windows_core::Result<Resources> {
    Application::Current()?.Resources()?.cast::<Resources>()
}

fn lookup(resources: &Resources, key: &str) -> windows_core::Result<IInspectable> {
    let key: IInspectable = IReference::<HSTRING>::from(HSTRING::from(key)).into();
    resources.Lookup(&key)
}

/// Resolves a theme brush resource to a flat color: solid brushes carry the
/// color directly, acrylic brushes expose it as their tint.
fn brush_color(resources: &Resources, key: &str) -> ResolvedColor {
    let value = lookup(resources, key)
        .unwrap_or_else(|error| panic!("WinUI theme resource {key} missing: {error}"));
    if let Ok(solid) = value.cast::<SolidColorBrush>() {
        return winui_color(solid.Color().expect("SolidColorBrush::Color"));
    }
    if let Ok(acrylic) = value.cast::<AcrylicBrush>() {
        return winui_color(acrylic.TintColor().expect("AcrylicBrush::TintColor"));
    }
    panic!("WinUI theme resource {key} is not a color brush");
}

fn winui_color(color: Color) -> ResolvedColor {
    let mut resolved = ResolvedColor::from_srgb(Srgb::new_u8(color.r, color.g, color.b));
    resolved.opacity = f32::from(color.a) / 255.0;
    resolved
}

/// Extracts size and weight from a `TextBlock` theme style (`BodyTextBlockStyle`
/// and friends). Setters the style does not declare fall back to the standard
/// base font-size resource and `Normal` weight — the same defaults XAML applies.
#[allow(clippy::cast_possible_truncation, reason = "DIP sizes fit f32")]
fn font(resources: &Resources, key: &str) -> ResolvedFont {
    let style: Style = lookup(resources, key)
        .unwrap_or_else(|error| panic!("WinUI text style {key} missing: {error}"))
        .cast()
        .expect("text style resource is a Style");
    let setters: windows_collections::IVector<SetterBase> = style
        .Setters()
        .expect("Style::Setters")
        .cast()
        .expect("SetterBaseCollection is an IVector");
    let size_property = TextElement::FontSizeProperty().expect("TextElement::FontSizeProperty");
    let weight_property =
        TextElement::FontWeightProperty().expect("TextElement::FontWeightProperty");

    let mut size = None;
    let mut weight = None;
    for base in &setters {
        let Ok(setter) = base.cast::<Setter>() else {
            continue;
        };
        let property = setter.Property().expect("Setter::Property");
        if property == size_property {
            size = Some(unbox::<f64>(&setter.Value().expect("Setter::Value"), key));
        } else if property == weight_property {
            let raw =
                unbox::<crate::bindings::FontWeight>(&setter.Value().expect("Setter::Value"), key);
            weight = Some(font_weight(raw.weight));
        }
    }

    let size = size.unwrap_or_else(|| {
        unbox::<f64>(
            &lookup(resources, "ControlContentThemeFontSize")
                .expect("ControlContentThemeFontSize resource"),
            key,
        )
    });
    ResolvedFont::new(size as f32, weight.unwrap_or(FontWeight::Normal))
}

fn unbox<T: windows_core::RuntimeType + 'static>(value: &IInspectable, key: &str) -> T {
    value
        .cast::<IReference<T>>()
        .and_then(|reference| reference.Value())
        .unwrap_or_else(|error| {
            panic!("WinUI resource {key} has an unexpected value type: {error}")
        })
}

/// Maps an `OpenType` weight (100-900) onto `WaterUI`'s weight steps.
fn font_weight(weight: u16) -> FontWeight {
    const STEPS: [FontWeight; 9] = [
        FontWeight::Thin,
        FontWeight::UltraLight,
        FontWeight::Light,
        FontWeight::Normal,
        FontWeight::Medium,
        FontWeight::SemiBold,
        FontWeight::Bold,
        FontWeight::UltraBold,
        FontWeight::Black,
    ];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = (weight.saturating_add(50) / 100).clamp(1, 9) as usize - 1;
    STEPS[index]
}
