//! Basic `WinUI` controls: button, toggle, slider, text entry, stepper,
//! progress.

use std::cell::RefCell;

use nami::Signal;
use waterui::component::progress::{ProgressConfig, ProgressStyle};
use waterui_controls::button::{ButtonConfig, ButtonStyle};
use waterui_controls::slider::SliderConfig;
use waterui_controls::stepper::StepperConfig;
use waterui_controls::text_field::ResolvedTextFieldConfig;
use waterui_controls::toggle::{ToggleConfig, ToggleStyle};
use waterui_core::{Environment, Native};
use waterui_form::secure::SecureFieldConfig;
use waterui_text::styled::StyledStr;
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{store_event_revoker, store_watcher_guard, store_watcher_guards};

impl WinUiComponent for Native<ButtonConfig> {
    /// Renders `Button` — or `HyperlinkButton` for frameless styles.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        let label = renderer.render(config.label, env);

        // `BoxedAction` is `FnMut`; the Click delegate is `Fn`, so it goes
        // behind a `RefCell` on the UI thread.
        let action = RefCell::new(config.action);
        let env_for_click = env.clone();
        let wire_click = move |element: &UIElement| {
            element
                .cast::<ButtonBase>()
                .expect("button is a ButtonBase")
                .Click(move |_, _| {
                    (action.borrow_mut())(&env_for_click);
                })
                .expect("ButtonBase::Click")
        };

        let element: FrameworkElement = match config.style {
            ButtonStyle::Plain | ButtonStyle::Borderless | ButtonStyle::Link => {
                let button = HyperlinkButton::new().expect("HyperlinkButton::new");
                button
                    .cast::<ContentControl>()
                    .expect("ContentControl")
                    .SetContent(&label)
                    .expect("HyperlinkButton::SetContent");
                let revoker = wire_click(&button.cast::<UIElement>().expect("UIElement"));
                let element: FrameworkElement = button
                    .cast()
                    .expect("HyperlinkButton is a FrameworkElement");
                store_event_revoker(&element, revoker);
                element
            }
            ButtonStyle::Automatic | ButtonStyle::Bordered | ButtonStyle::BorderedProminent => {
                let button = Button::new().expect("Button::new");
                button
                    .cast::<ContentControl>()
                    .expect("ContentControl")
                    .SetContent(&label)
                    .expect("Button::SetContent");
                let revoker = wire_click(&button.cast::<UIElement>().expect("UIElement"));
                let element: FrameworkElement =
                    button.cast().expect("Button is a FrameworkElement");
                store_event_revoker(&element, revoker);
                element
            }
            _ => panic!(
                "unsupported ButtonStyle on the WinUI backend: {:?}",
                config.style
            ),
        };
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for Native<ToggleConfig> {
    /// Renders a `ToggleSwitch` (switch style) or `CheckBox`, two-way bound.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        match config.style {
            ToggleStyle::Switch | ToggleStyle::Automatic => {
                render_toggle_switch(config, env, renderer)
            }
            ToggleStyle::Checkbox => render_checkbox(config, env, renderer),
            _ => panic!("unsupported ToggleStyle on the WinUI backend"),
        }
    }
}

fn render_toggle_switch(
    config: ToggleConfig,
    env: &Environment,
    renderer: &mut WinUiRenderer,
) -> UIElement {
    let switch = ToggleSwitch::new().expect("ToggleSwitch::new");
    let label = renderer.render(config.label, env);
    switch.SetHeader(&label).expect("ToggleSwitch::SetHeader");

    let binding = config.toggle;
    switch
        .SetIsOn(binding.get())
        .expect("ToggleSwitch::SetIsOn");

    let queue = renderer.executor().queue().clone();
    let weak = switch.downgrade().expect("ToggleSwitch supports weak refs");
    let guard = binding.watch(move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(switch) = weak.upgrade()
                && switch.IsOn().is_ok_and(|on| on != value)
            {
                switch.SetIsOn(value).expect("ToggleSwitch::SetIsOn");
            }
        });
    });

    let revoker = switch
        .Toggled(move |sender, _| {
            if let Ok(sender) = sender.ok() {
                let switch: ToggleSwitch = sender.cast().expect("ToggleSwitch");
                binding.set(switch.IsOn().unwrap_or(false));
            }
        })
        .expect("ToggleSwitch::Toggled");

    let element: FrameworkElement = switch.cast().expect("ToggleSwitch is a FrameworkElement");
    store_watcher_guard(&element, guard);
    store_event_revoker(&element, revoker);
    element.cast().expect("FrameworkElement is a UIElement")
}

fn render_checkbox(
    config: ToggleConfig,
    env: &Environment,
    renderer: &mut WinUiRenderer,
) -> UIElement {
    let checkbox = CheckBox::new().expect("CheckBox::new");
    let label = renderer.render(config.label, env);
    checkbox
        .cast::<ContentControl>()
        .expect("ContentControl")
        .SetContent(&label)
        .expect("CheckBox::SetContent");

    let binding = config.toggle;
    let toggle: ToggleButton = checkbox.cast().expect("CheckBox is a ToggleButton");
    toggle
        .SetIsChecked(Some(binding.get()))
        .expect("ToggleButton::SetIsChecked");

    let queue = renderer.executor().queue().clone();
    let weak = toggle.downgrade().expect("ToggleButton supports weak refs");
    let guard = binding.watch(move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(toggle) = weak.upgrade()
                && toggle.IsChecked().is_ok_and(|on| on != value)
            {
                toggle
                    .SetIsChecked(Some(value))
                    .expect("ToggleButton::SetIsChecked");
            }
        });
    });

    let binding_checked = binding.clone();
    let checked = toggle
        .Checked(move |_, _| {
            binding_checked.set(true);
        })
        .expect("ToggleButton::Checked");
    let unchecked = toggle
        .Unchecked(move |_, _| {
            binding.set(false);
        })
        .expect("ToggleButton::Unchecked");

    let element: FrameworkElement = checkbox.cast().expect("CheckBox is a FrameworkElement");
    store_watcher_guard(&element, guard);
    store_event_revoker(&element, checked);
    store_event_revoker(&element, unchecked);
    element.cast().expect("FrameworkElement is a UIElement")
}

impl WinUiComponent for Native<SliderConfig> {
    /// Renders a `Slider` with optional min/max labels, two-way bound.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let slider = Slider::new().expect("Slider::new");
        let range: RangeBase = slider.cast().expect("Slider is a RangeBase");
        range
            .SetMinimum(*config.range.start())
            .expect("RangeBase::SetMinimum");
        range
            .SetMaximum(*config.range.end())
            .expect("RangeBase::SetMaximum");

        let binding = config.value;
        range.SetValue(binding.get()).expect("RangeBase::SetValue");

        let queue = renderer.executor().queue().clone();
        let weak = range.downgrade().expect("RangeBase supports weak refs");
        let guard = binding.watch(move |ctx| {
            let value = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(range) = weak.upgrade()
                    && range
                        .Value()
                        .is_ok_and(|current| (current - value).abs() > f64::EPSILON)
                {
                    range.SetValue(value).expect("RangeBase::SetValue");
                }
            });
        });

        let binding_for_event = binding.clone();
        let revoker = range
            .ValueChanged(move |_, args| {
                if let Ok(args) = args.ok() {
                    let value = args.NewValue().unwrap_or_else(|_| binding_for_event.get());
                    if (binding_for_event.get() - value).abs() > f64::EPSILON {
                        binding_for_event.set(value);
                    }
                }
            })
            .expect("RangeBase::ValueChanged");

        // Layout: label on top, then [min-label | slider | max-label].
        let root = StackPanel::new().expect("StackPanel::new");
        root.SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        root.SetSpacing(4.0).expect("StackPanel::SetSpacing");
        let children = crate::util::children(&root);
        children
            .Append(&renderer.render(config.label, env))
            .expect("Children::Append");

        let row = StackPanel::new().expect("StackPanel::new");
        row.SetOrientation(Orientation::Horizontal)
            .expect("StackPanel::SetOrientation");
        row.SetSpacing(8.0).expect("StackPanel::SetSpacing");
        let row_children = crate::util::children(&row);
        row_children
            .Append(&renderer.render_any(config.min_value_label, env))
            .expect("Children::Append");
        row_children
            .Append(&slider.cast::<UIElement>().expect("Slider is a UIElement"))
            .expect("Children::Append");
        row_children
            .Append(&renderer.render_any(config.max_value_label, env))
            .expect("Children::Append");
        children
            .Append(&row.cast::<UIElement>().expect("StackPanel is a UIElement"))
            .expect("Children::Append");

        let element: FrameworkElement = root.cast().expect("StackPanel is a FrameworkElement");
        store_watcher_guard(&element, guard);
        store_event_revoker(&element, revoker);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for Native<ResolvedTextFieldConfig> {
    /// Renders a `TextBox` two-way bound to the `StyledStr` binding as plain
    /// text.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let textbox = TextBox::new().expect("TextBox::new");
        textbox
            .SetPlaceholderText(config.prompt.content.get().to_plain().as_str())
            .expect("TextBox::SetPlaceholderText");
        if config.line_limit.is_none() {
            textbox
                .SetAcceptsReturn(true)
                .expect("TextBox::SetAcceptsReturn");
        }

        let binding = config.value;
        textbox
            .SetText(binding.get().to_plain().as_str())
            .expect("TextBox::SetText");

        let queue = renderer.executor().queue().clone();
        let weak = textbox.downgrade().expect("TextBox supports weak refs");
        let value_guard = binding.watch(move |ctx| {
            let value = ctx.into_value().to_plain().to_string();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(textbox) = weak.upgrade()
                    && textbox.Text().is_ok_and(|t| t != value)
                {
                    textbox.SetText(&value).expect("TextBox::SetText");
                }
            });
        });

        let prompt = config.prompt.content;
        let queue = renderer.executor().queue().clone();
        let weak = textbox.downgrade().expect("TextBox supports weak refs");
        let prompt_guard = prompt.watch(move |ctx| {
            let prompt_text = ctx.into_value().to_plain().to_string();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(textbox) = weak.upgrade() {
                    textbox
                        .SetPlaceholderText(&prompt_text)
                        .expect("TextBox::SetPlaceholderText");
                }
            });
        });

        let binding_for_event = binding.clone();
        let revoker = textbox
            .TextChanged(move |sender, _| {
                if let Ok(sender) = sender.ok()
                    && let Ok(textbox) = sender.cast::<TextBox>()
                    && let Ok(text) = textbox.Text()
                    && text != binding_for_event.get().to_plain()
                {
                    binding_for_event.set(StyledStr::plain(text));
                }
            })
            .expect("TextBox::TextChanged");

        // Label above the entry, matching the GTK layout.
        let root = StackPanel::new().expect("StackPanel::new");
        root.SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        root.SetSpacing(4.0).expect("StackPanel::SetSpacing");
        let children = crate::util::children(&root);
        children
            .Append(&renderer.render(config.label, env))
            .expect("Children::Append");
        children
            .Append(&textbox.cast::<UIElement>().expect("TextBox is a UIElement"))
            .expect("Children::Append");

        let element: FrameworkElement = root.cast().expect("StackPanel is a FrameworkElement");
        store_watcher_guards(&element, [value_guard, prompt_guard]);
        store_event_revoker(&element, revoker);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for Native<SecureFieldConfig> {
    /// Renders a `PasswordBox` two-way bound to the secure string.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let password = PasswordBox::new().expect("PasswordBox::new");
        password
            .SetPassword(config.value.get().expose())
            .expect("PasswordBox::SetPassword");
        password
            .SetPasswordRevealMode(PasswordRevealMode::Peek)
            .expect("PasswordBox::SetPasswordRevealMode");

        let binding = config.value;
        let queue = renderer.executor().queue().clone();
        let weak = password
            .downgrade()
            .expect("PasswordBox supports weak refs");
        let guard = binding.watch(move |ctx| {
            let value = ctx.into_value().expose().to_string();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(password) = weak.upgrade()
                    && password.Password().is_ok_and(|p| p != value)
                {
                    password
                        .SetPassword(&value)
                        .expect("PasswordBox::SetPassword");
                }
            });
        });

        let binding_for_event = binding.clone();
        let revoker = password
            .PasswordChanged(move |sender, _| {
                if let Ok(sender) = sender.ok()
                    && let Ok(password) = sender.cast::<PasswordBox>()
                    && let Ok(text) = password.Password()
                {
                    binding_for_event.set(text.parse().expect("Secure FromStr is infallible"));
                }
            })
            .expect("PasswordBox::PasswordChanged");

        let root = StackPanel::new().expect("StackPanel::new");
        root.SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        root.SetSpacing(4.0).expect("StackPanel::SetSpacing");
        let children = crate::util::children(&root);
        children
            .Append(&renderer.render(config.label, env))
            .expect("Children::Append");
        children
            .Append(
                &password
                    .cast::<UIElement>()
                    .expect("PasswordBox is a UIElement"),
            )
            .expect("Children::Append");

        let element: FrameworkElement = root.cast().expect("StackPanel is a FrameworkElement");
        store_watcher_guard(&element, guard);
        store_event_revoker(&element, revoker);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for Native<StepperConfig> {
    /// Renders a `NumberBox` with inline spin buttons.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let number = NumberBox::new().expect("NumberBox::new");
        number
            .SetSpinButtonPlacementMode(NumberBoxSpinButtonPlacementMode::Inline)
            .expect("NumberBox::SetSpinButtonPlacementMode");
        number
            .SetMinimum(f64::from(*config.range.start()))
            .expect("NumberBox::SetMinimum");
        number
            .SetMaximum(f64::from(*config.range.end()))
            .expect("NumberBox::SetMaximum");
        number
            .SetSmallChange(f64::from(config.step.get()))
            .expect("NumberBox::SetSmallChange");

        let binding = config.value;
        number
            .SetValue(f64::from(binding.get()))
            .expect("NumberBox::SetValue");

        let queue = renderer.executor().queue().clone();
        let weak = number.downgrade().expect("NumberBox supports weak refs");
        let guard = binding.watch(move |ctx| {
            let value = f64::from(ctx.into_value());
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(number) = weak.upgrade()
                    && number
                        .Value()
                        .is_ok_and(|v| (v - value).abs() > f64::EPSILON)
                {
                    number.SetValue(value).expect("NumberBox::SetValue");
                }
            });
        });

        let step_signal = config.step;
        let queue = renderer.executor().queue().clone();
        let weak = number.downgrade().expect("NumberBox supports weak refs");
        let step_guard = step_signal.watch(move |ctx| {
            let step = f64::from(ctx.into_value());
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(number) = weak.upgrade() {
                    number
                        .SetSmallChange(step)
                        .expect("NumberBox::SetSmallChange");
                }
            });
        });

        let binding_for_event = binding.clone();
        let revoker = number
            .ValueChanged(move |_, args| {
                if let Ok(args) = args.ok()
                    && let Ok(value) = args.NewValue()
                    && value.is_finite()
                {
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_lossless,
                        reason = "stepper values are bounded by the configured i32 range"
                    )]
                    binding_for_event.set(value.round() as i32);
                }
            })
            .expect("NumberBox::ValueChanged");

        let root = StackPanel::new().expect("StackPanel::new");
        root.SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        root.SetSpacing(4.0).expect("StackPanel::SetSpacing");
        let children = crate::util::children(&root);
        children
            .Append(&renderer.render(config.label, env))
            .expect("Children::Append");
        children
            .Append(
                &number
                    .cast::<UIElement>()
                    .expect("NumberBox is a UIElement"),
            )
            .expect("Children::Append");

        let element: FrameworkElement = root.cast().expect("StackPanel is a FrameworkElement");
        store_watcher_guards(&element, [guard, step_guard]);
        store_event_revoker(&element, revoker);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for Native<ProgressConfig> {
    /// Renders `ProgressBar` (linear) or `ProgressRing` (circular/loading).
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let root = StackPanel::new().expect("StackPanel::new");
        root.SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        root.SetSpacing(4.0).expect("StackPanel::SetSpacing");
        let children = crate::util::children(&root);
        children
            .Append(&renderer.render_any(config.label, env))
            .expect("Children::Append");

        let queue = renderer.executor().queue().clone();
        let guard = match config.style {
            ProgressStyle::Linear => {
                let bar = ProgressBar::new().expect("ProgressBar::new");
                let bar_range: RangeBase = bar.cast().expect("ProgressBar is a RangeBase");
                bar_range.SetMinimum(0.0).expect("RangeBase::SetMinimum");
                bar_range.SetMaximum(1.0).expect("RangeBase::SetMaximum");
                children
                    .Append(&bar.cast::<UIElement>().expect("ProgressBar is a UIElement"))
                    .expect("Children::Append");
                let weak = bar.downgrade().expect("ProgressBar supports weak refs");
                config.value.watch(move |ctx| {
                    let value = ctx.into_value();
                    let weak = weak.clone();
                    let queue = queue.clone();
                    enqueue_on_ui_thread(&queue, move || {
                        if let Some(bar) = weak.upgrade() {
                            bar.SetIsIndeterminate(value.is_nan())
                                .expect("ProgressBar::SetIsIndeterminate");
                            if value.is_finite() {
                                bar.cast::<RangeBase>()
                                    .expect("RangeBase")
                                    .SetValue(value.clamp(0.0, 1.0))
                                    .expect("RangeBase::SetValue");
                            }
                        }
                    });
                })
            }
            ProgressStyle::Circular | ProgressStyle::Loading => {
                let ring = ProgressRing::new().expect("ProgressRing::new");
                ring.SetIsActive(true).expect("ProgressRing::SetIsActive");
                ring.SetMinimum(0.0).expect("ProgressRing::SetMinimum");
                ring.SetMaximum(1.0).expect("ProgressRing::SetMaximum");
                children
                    .Append(
                        &ring
                            .cast::<UIElement>()
                            .expect("ProgressRing is a UIElement"),
                    )
                    .expect("Children::Append");
                let weak = ring.downgrade().expect("ProgressRing supports weak refs");
                config.value.watch(move |ctx| {
                    let value = ctx.into_value();
                    let weak = weak.clone();
                    let queue = queue.clone();
                    enqueue_on_ui_thread(&queue, move || {
                        if let Some(ring) = weak.upgrade() {
                            ring.SetIsIndeterminate(value.is_nan())
                                .expect("ProgressRing::SetIsIndeterminate");
                            if value.is_finite() {
                                ring.SetValue(value.clamp(0.0, 1.0))
                                    .expect("ProgressRing::SetValue");
                            }
                        }
                    });
                })
            }
            _ => panic!("unsupported ProgressStyle on the WinUI backend"),
        };
        children
            .Append(&renderer.render_any(config.value_label, env))
            .expect("Children::Append");

        let element: FrameworkElement = root.cast().expect("StackPanel is a FrameworkElement");
        store_watcher_guard(&element, guard);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}
