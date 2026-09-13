//! Text rendering: `StyledStr` chunks become `TextBlock` inline runs.

use nami::Signal;
use waterui_core::layout::HorizontalAlignment;
use waterui_core::{Environment, Native};
use waterui_text::TextConfig;
use waterui_text::font::{FontWeight as WaterUiFontWeight, ResolvedFont};
use waterui_text::styled::{Style, StyledStr};
use windows_core::Interface;

use crate::bindings::FontWeight as WinUiFontWeight;
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{framework, solid_brush, store_watcher_guards};

impl WinUiComponent for Native<TextConfig> {
    /// Renders `TextConfig` as a `TextBlock` with per-run styling.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        let block = TextBlock::new().expect("TextBlock::new");
        block
            .SetTextWrapping(TextWrapping::WrapWholeWords)
            .expect("TextBlock::SetTextWrapping");

        if let Some(limit) = config.line_limit {
            block
                .SetMaxLines(i32::try_from(limit.get()).unwrap_or(i32::MAX))
                .expect("TextBlock::SetMaxLines");
            block
                .SetTextTrimming(TextTrimming::CharacterEllipsis)
                .expect("TextBlock::SetTextTrimming");
        }

        let queue = renderer.executor().queue().clone();

        // Content updates rebuild the inline runs.
        let weak = block.downgrade().expect("TextBlock supports weak refs");
        let env_for_watch = env.clone();
        let alignment_signal = config.paragraph_alignment.clone();
        let content_guard = config.content.watch(move |ctx| {
            let content = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            let env = env_for_watch.clone();
            let alignment = alignment_signal.get();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(block) = weak.upgrade() {
                    apply_styled_content(&block, &content, alignment, &env);
                }
            });
        });

        apply_styled_content(
            &block,
            &config.content.get(),
            config.paragraph_alignment.get(),
            env,
        );

        // Alignment updates retarget `TextAlignment`.
        let queue = renderer.executor().queue().clone();
        let weak = block.downgrade().expect("TextBlock supports weak refs");
        let alignment_guard = config.paragraph_alignment.watch(move |ctx| {
            let alignment = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(block) = weak.upgrade() {
                    block
                        .SetTextAlignment(text_alignment(alignment))
                        .expect("TextBlock::SetTextAlignment");
                }
            });
        });

        store_watcher_guards(
            &framework(&block.cast().expect("TextBlock is a UIElement")),
            [content_guard, alignment_guard],
        );
        block.cast().expect("TextBlock is a UIElement")
    }
}

/// Fills `block`'s inline runs from a styled string.
fn apply_styled_content(
    block: &TextBlock,
    content: &StyledStr,
    alignment: HorizontalAlignment,
    env: &Environment,
) {
    let inlines = crate::util::inlines(&block.Inlines().expect("TextBlock::Inlines"));
    inlines.Clear().expect("InlineCollection::Clear");
    for (text, style) in content.chunks() {
        let run = Run::new().expect("Run::new");
        run.SetText(text.as_str()).expect("Run::SetText");
        apply_style(&run, style, env);
        inlines
            .Append(&run.cast::<Inline>().expect("Run is an Inline"))
            .expect("InlineCollection::Append");
    }
    block
        .SetTextAlignment(text_alignment(alignment))
        .expect("TextBlock::SetTextAlignment");
}

/// Projects a `Style` chunk onto a `Run`'s text properties.
fn apply_style(run: &Run, style: &Style, env: &Environment) {
    // Font and foreground setters live on `ITextElement`, not `Run`.
    let text = run.cast::<ITextElement>().expect("Run is a TextElement");
    let resolved: ResolvedFont = style.font.resolve(env).get();
    text.SetFontSize(f64::from(resolved.size.max(1.0)))
        .expect("TextElement::SetFontSize");
    text.SetFontWeight(WinUiFontWeight {
        weight: font_weight_value(resolved.weight),
    })
    .expect("TextElement::SetFontWeight");
    if let Some(family) = resolved.family.as_ref() {
        let family = FontFamily::CreateInstanceWithName(family.as_str())
            .expect("FontFamily::CreateInstanceWithName");
        text.SetFontFamily(&family)
            .expect("TextElement::SetFontFamily");
    }
    if style.italic {
        text.SetFontStyle(FontStyle::Italic)
            .expect("TextElement::SetFontStyle");
    }
    let mut decorations = TextDecorations::None;
    if style.underline {
        decorations |= TextDecorations::Underline;
    }
    if style.strikethrough {
        decorations |= TextDecorations::Strikethrough;
    }
    if decorations != TextDecorations::None {
        text.SetTextDecorations(decorations)
            .expect("TextElement::SetTextDecorations");
    }
    if let Some(foreground) = &style.foreground {
        let resolved = foreground.resolve(env).get();
        let brush = solid_brush(&resolved).expect("SolidColorBrush");
        text.SetForeground(&brush)
            .expect("TextElement::SetForeground");
    }
}

fn text_alignment(alignment: HorizontalAlignment) -> TextAlignment {
    if alignment == HorizontalAlignment::Leading {
        TextAlignment::Left
    } else if alignment == HorizontalAlignment::Center {
        TextAlignment::Center
    } else if alignment == HorizontalAlignment::Trailing {
        TextAlignment::Right
    } else {
        panic!("unsupported horizontal alignment for WinUI text: {alignment:?}")
    }
}

/// `FontWeight` is a plain `u16` struct; map WaterUI weights to OpenType
/// weight values.
const fn font_weight_value(weight: WaterUiFontWeight) -> u16 {
    match weight {
        WaterUiFontWeight::Thin => 100,
        WaterUiFontWeight::UltraLight => 200,
        WaterUiFontWeight::Light => 300,
        WaterUiFontWeight::Normal => 400,
        WaterUiFontWeight::Medium => 500,
        WaterUiFontWeight::SemiBold => 600,
        WaterUiFontWeight::Bold => 700,
        WaterUiFontWeight::UltraBold => 800,
        WaterUiFontWeight::Black => 900,
    }
}
