//! Picker controls: option pickers, date/time pickers, color picker.

use std::cell::RefCell;
use std::rc::Rc;

use nami::Signal;
use waterui_core::id::Id;
use waterui_core::{Environment, Native};
use waterui_form::picker::color::ColorPickerConfig;
use waterui_form::picker::date::{Date, DatePickerConfig, DatePickerType, DateTime};
use waterui_form::picker::multi_date::MultiDatePickerConfig;
use waterui_form::picker::{PickerConfig, PickerItem, PickerStyle};
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{
    framework, resolved_color_to_winui, store_event_revoker, store_watcher_guards,
    subscribe_then_get,
};

/// Ticks between the `WinRT` epoch (1601-01-01 UTC) and the Unix epoch.
const WINRT_UNIX_EPOCH_OFFSET: i64 = 116_444_736_000_000_000 / 10;

/// jiff civil `DateTime` → `windows_time::DateTime`, interpreting the civil
/// value in the system time zone (matching GTK's `from_local`).
///
/// `DatePickable::full_range` bounds unconstrained pickers with civil
/// `DateTime::{MIN, MAX}`, which `jiff::Timestamp` cannot represent; those
/// sentinels clamp to the edges of the `WinRT` `DateTime` range.
fn to_winrt_datetime(value: DateTime) -> windows_time::DateTime {
    let unix_100ns = value
        .to_zoned(jiff_tz())
        .ok()
        .and_then(|zoned| i64::try_from(zoned.timestamp().as_nanosecond() / 100).ok())
        .unwrap_or_else(|| {
            if value < DateTime::constant(1970, 1, 1, 0, 0, 0, 0) {
                i64::MIN
            } else {
                i64::MAX
            }
        });
    windows_time::DateTime {
        universal_time: unix_100ns
            .saturating_add(WINRT_UNIX_EPOCH_OFFSET)
            .clamp(0, i64::MAX),
    }
}

/// `windows_time::DateTime` → jiff civil `DateTime` in the system zone.
fn from_winrt_datetime(value: windows_time::DateTime) -> DateTime {
    let unix_nanos = i128::from(value.universal_time - WINRT_UNIX_EPOCH_OFFSET) * 100;
    let timestamp = jiff_timestamp(unix_nanos);
    timestamp.to_zoned(jiff_tz()).datetime()
}

fn jiff_tz() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::system()
}

fn jiff_timestamp(nanos: i128) -> jiff::Timestamp {
    jiff::Timestamp::from_nanosecond(nanos).expect("WinRT DateTime must fit jiff::Timestamp")
}

/// jiff civil `Date` → `windows_time::DateTime` at local midnight.
fn to_winrt_date(value: Date) -> windows_time::DateTime {
    to_winrt_datetime(value.at(0, 0, 0, 0))
}

fn from_winrt_date(value: windows_time::DateTime) -> Date {
    from_winrt_datetime(value).date()
}

/// Renders a `Text` view's plain content into a `TextBlock` for use as a
/// list/picker item.
fn item_element(item: &PickerItem<Id>, env: &Environment) -> TextBlock {
    let text = item
        .content
        .resolve(env)
        .content
        .snapshot()
        .to_plain()
        .to_string();
    let block = TextBlock::new().expect("TextBlock::new");
    block.SetText(text.as_str()).expect("TextBlock::SetText");
    block
}

impl WinUiComponent for Native<PickerConfig> {
    /// Renders the picker as `ComboBox` (menu/automatic), `RadioButtons`
    /// (radio), or `SelectorBar` (segmented).
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let ids: Rc<RefCell<Vec<Id>>> = Rc::new(RefCell::new(Vec::new()));
        let element = match config.style {
            PickerStyle::Radio => render_radio_buttons(&config, &ids, env, renderer),
            PickerStyle::Segmented => render_selector_bar(&config, &ids, env, renderer),
            PickerStyle::Automatic | PickerStyle::Menu => {
                render_combo_box(&config, &ids, env, renderer)
            }
            other => panic!("unsupported PickerStyle for the WinUI backend: {other:?}"),
        };

        // `Label::resolve` consumes the label, so it must run after the
        // helpers that borrow `config`.
        let accessibility = config
            .label
            .resolve(env)
            .accessibility_label()
            .snapshot()
            .to_plain()
            .to_string();
        AutomationProperties::SetName(&framework(&element), accessibility.as_str())
            .expect("AutomationProperties::SetName");
        element
    }
}

#[allow(clippy::too_many_lines)] // flat registration/wiring code
fn render_combo_box(
    config: &PickerConfig,
    ids: &Rc<RefCell<Vec<Id>>>,
    env: &Environment,
    renderer: &WinUiRenderer,
) -> UIElement {
    let combo = ComboBox::new().expect("ComboBox::new");
    let items_collection = combo
        .cast::<ItemsControl>()
        .expect("ComboBox is an ItemsControl")
        .Items()
        .expect("ItemsControl::Items");

    let install_items = |list: &[PickerItem<Id>]| {
        items_collection.Clear().expect("ItemCollection::Clear");
        for item in list {
            let element = item_element(item, env);
            items_collection
                .Append(
                    &element
                        .cast::<windows_core::IInspectable>()
                        .expect("IInspectable"),
                )
                .expect("ItemCollection::Append");
        }
        *ids.borrow_mut() = list.iter().map(|item| item.tag).collect();
        let current = config.selection.snapshot();
        let index = ids
            .borrow()
            .iter()
            .position(|id| *id == current)
            .map_or(-1, |index| i32::try_from(index).expect("index fits i32"));
        combo
            .cast::<Selector>()
            .expect("ComboBox is a Selector")
            .SetSelectedIndex(index)
            .expect("Selector::SetSelectedIndex");
    };
    install_items(&config.items.snapshot());

    // native -> binding
    let selection = config.selection.clone();
    let ids_for_event = ids.clone();
    let revoker = combo
        .cast::<Selector>()
        .expect("ComboBox is a Selector")
        .SelectionChanged(move |sender, _args| {
            let combo = sender
                .ok()
                .expect("SelectionChanged sender")
                .cast::<ComboBox>()
                .expect("sender is the ComboBox");
            let index = combo
                .cast::<Selector>()
                .expect("ComboBox is a Selector")
                .SelectedIndex()
                .expect("Selector::SelectedIndex");
            let ids_ref = ids_for_event.borrow();
            if index >= 0
                && let Some(id) = ids_ref
                    .as_slice()
                    .get(usize::try_from(index).expect("index non-negative"))
                    .copied()
                && selection.snapshot() != id
            {
                selection.set(id);
            }
        })
        .expect("Selector::SelectionChanged");
    store_event_revoker(
        &framework(&combo.cast().expect("ComboBox is a UIElement")),
        revoker,
    );

    // binding -> native
    let queue = renderer.executor().queue().clone();
    let weak = combo.downgrade().expect("ComboBox supports weak refs");
    let ids_for_watch = ids.clone();
    let selection_guard = {
        let binding = config.selection.clone();
        subscribe_then_get(&binding, move |ctx| {
            let value = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            let ids = ids_for_watch.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(combo) = weak.upgrade() {
                    let selector = combo.cast::<Selector>().expect("ComboBox is a Selector");
                    let index = ids
                        .borrow()
                        .iter()
                        .position(|id| *id == value)
                        .map_or(-1, |index| i32::try_from(index).expect("index fits i32"));
                    if selector.SelectedIndex().expect("SelectedIndex") != index {
                        selector.SetSelectedIndex(index).expect("SetSelectedIndex");
                    }
                }
            });
        })
        .1
    };

    // items signal -> native model
    let weak = combo.downgrade().expect("ComboBox supports weak refs");
    let queue = renderer.executor().queue().clone();
    let env_for_items = env.clone();
    let ids_for_items = ids.clone();
    let items_guard = config.items.watch(move |ctx| {
        let list = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        let env = env_for_items.clone();
        let ids = ids_for_items.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(combo) = weak.upgrade() {
                let items_collection = combo
                    .cast::<ItemsControl>()
                    .expect("ComboBox is an ItemsControl")
                    .Items()
                    .expect("ItemsControl::Items");
                items_collection.Clear().expect("ItemCollection::Clear");
                for item in &list {
                    let element = item_element(item, &env);
                    items_collection
                        .Append(
                            &element
                                .cast::<windows_core::IInspectable>()
                                .expect("IInspectable"),
                        )
                        .expect("ItemCollection::Append");
                }
                *ids.borrow_mut() = list.iter().map(|item| item.tag).collect();
            }
        });
    });

    store_watcher_guards(
        &framework(&combo.cast().expect("ComboBox is a UIElement")),
        [selection_guard, items_guard],
    );
    combo.cast().expect("ComboBox is a UIElement")
}

fn render_radio_buttons(
    config: &PickerConfig,
    ids: &Rc<RefCell<Vec<Id>>>,
    env: &Environment,
    renderer: &WinUiRenderer,
) -> UIElement {
    let radio = RadioButtons::new().expect("RadioButtons::new");
    // `RadioButtons` is a `Control`, not an `ItemsControl`; its own `Items`
    // returns the live `IVector<IInspectable>` backing the control.
    let items_collection: windows_collections::IVector<windows_core::IInspectable> =
        crate::util::vector(&radio.Items().expect("RadioButtons::Items"));

    let install_items = |list: &[PickerItem<Id>]| {
        items_collection.Clear().expect("ItemCollection::Clear");
        for item in list {
            let element = item_element(item, env);
            items_collection
                .Append(
                    &element
                        .cast::<windows_core::IInspectable>()
                        .expect("IInspectable"),
                )
                .expect("ItemCollection::Append");
        }
        *ids.borrow_mut() = list.iter().map(|item| item.tag).collect();
        let current = config.selection.snapshot();
        let index = ids
            .borrow()
            .iter()
            .position(|id| *id == current)
            .map_or(-1, |index| i32::try_from(index).expect("index fits i32"));
        radio
            .SetSelectedIndex(index)
            .expect("RadioButtons::SetSelectedIndex");
    };
    install_items(&config.items.snapshot());

    let selection = config.selection.clone();
    let ids_for_event = ids.clone();
    let revoker = radio
        .SelectionChanged(move |sender, _args| {
            let radio = sender
                .ok()
                .expect("SelectionChanged sender")
                .cast::<RadioButtons>()
                .expect("sender is the RadioButtons");
            let index = radio.SelectedIndex().expect("RadioButtons::SelectedIndex");
            let ids_ref = ids_for_event.borrow();
            if index >= 0
                && let Some(id) = ids_ref
                    .as_slice()
                    .get(usize::try_from(index).expect("index non-negative"))
                    .copied()
                && selection.snapshot() != id
            {
                selection.set(id);
            }
        })
        .expect("RadioButtons::SelectionChanged");
    store_event_revoker(&framework(&radio.cast().expect("UIElement")), revoker);

    let queue = renderer.executor().queue().clone();
    let weak = radio.downgrade().expect("RadioButtons supports weak refs");
    let ids_for_watch = ids.clone();
    let selection_guard = subscribe_then_get(&config.selection, move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        let ids = ids_for_watch.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(radio) = weak.upgrade() {
                let index = ids
                    .borrow()
                    .iter()
                    .position(|id| *id == value)
                    .map_or(-1, |index| i32::try_from(index).expect("index fits i32"));
                if radio.SelectedIndex().expect("SelectedIndex") != index {
                    radio.SetSelectedIndex(index).expect("SetSelectedIndex");
                }
            }
        });
    })
    .1;

    store_watcher_guards(
        &framework(&radio.cast().expect("UIElement")),
        [selection_guard],
    );
    radio.cast().expect("RadioButtons is a UIElement")
}

fn render_selector_bar(
    config: &PickerConfig,
    ids: &Rc<RefCell<Vec<Id>>>,
    env: &Environment,
    renderer: &WinUiRenderer,
) -> UIElement {
    let bar = SelectorBar::new().expect("SelectorBar::new");
    let items_collection: windows_collections::IVector<SelectorBarItem> =
        crate::util::vector(&bar.Items().expect("SelectorBar::Items"));

    let install_items = |list: &[PickerItem<Id>]| {
        items_collection.Clear().expect("IVector::Clear");
        for item in list {
            let entry = SelectorBarItem::new().expect("SelectorBarItem::new");
            let text = item
                .content
                .resolve(env)
                .content
                .snapshot()
                .to_plain()
                .to_string();
            entry
                .SetText(text.as_str())
                .expect("SelectorBarItem::SetText");
            items_collection.Append(&entry).expect("IVector::Append");
        }
        *ids.borrow_mut() = list.iter().map(|item| item.tag).collect();
        let current = config.selection.snapshot();
        if let Some(index) = ids.borrow().iter().position(|id| *id == current) {
            let selected = items_collection
                .GetAt(u32::try_from(index).expect("index fits u32"))
                .expect("IVector::GetAt");
            bar.SetSelectedItem(&selected)
                .expect("SelectorBar::SetSelectedItem");
        }
    };
    install_items(&config.items.snapshot());

    let selection = config.selection.clone();
    let ids_for_event = ids.clone();
    let items_for_event = items_collection.clone();
    let revoker = bar
        .SelectionChanged(move |sender, _args| {
            let bar = sender
                .ok()
                .expect("SelectionChanged sender")
                .cast::<SelectorBar>()
                .expect("sender is the SelectorBar");
            let selected = bar.SelectedItem().expect("SelectorBar::SelectedItem");
            let mut index = 0u32;
            let found = items_for_event
                .IndexOf(&selected, &mut index)
                .expect("IVector::IndexOf")
                .then_some(index as usize);
            let ids_ref = ids_for_event.borrow();
            if let Some(index) = found
                && let Some(id) = ids_ref.as_slice().get(index).copied()
                && selection.snapshot() != id
            {
                selection.set(id);
            }
        })
        .expect("SelectorBar::SelectionChanged");
    store_event_revoker(&framework(&bar.cast().expect("UIElement")), revoker);

    let queue = renderer.executor().queue().clone();
    let weak = bar.downgrade().expect("SelectorBar supports weak refs");
    let ids_for_watch = ids.clone();
    let selection_guard = subscribe_then_get(&config.selection, move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        let ids = ids_for_watch.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(bar) = weak.upgrade()
                && let Some(index) = ids.borrow().iter().position(|id| *id == value)
            {
                let items: windows_collections::IVector<SelectorBarItem> =
                    crate::util::vector(&bar.Items().expect("SelectorBar::Items"));
                let item = items
                    .GetAt(u32::try_from(index).expect("index fits u32"))
                    .expect("IVector::GetAt");
                bar.SetSelectedItem(&item).expect("SetSelectedItem");
            }
        });
    })
    .1;

    store_watcher_guards(
        &framework(&bar.cast().expect("UIElement")),
        [selection_guard],
    );
    bar.cast().expect("SelectorBar is a UIElement")
}

impl WinUiComponent for Native<DatePickerConfig> {
    /// Renders a `CalendarDatePicker`, plus a `TimePicker` when the picker
    /// type includes a time component.
    #[allow(clippy::too_many_lines)] // flat registration/wiring code
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        let wants_time = !matches!(config.ty, DatePickerType::Date);

        let panel = StackPanel::new().expect("StackPanel::new");
        panel
            .SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        panel.SetSpacing(6.0).expect("StackPanel::SetSpacing");

        let label = renderer.render(config.label, env);
        crate::util::children(&panel)
            .Append(&label)
            .expect("Append label");

        let row = StackPanel::new().expect("StackPanel::new");
        row.SetOrientation(Orientation::Horizontal)
            .expect("StackPanel::SetOrientation");
        row.SetSpacing(8.0).expect("StackPanel::SetSpacing");

        let date_picker = CalendarDatePicker::new().expect("CalendarDatePicker::new");
        date_picker
            .SetDate(Some(to_winrt_datetime(config.value.snapshot())))
            .expect("CalendarDatePicker::SetDate");
        date_picker
            .SetMinDate(to_winrt_datetime(*config.range.start()))
            .expect("CalendarDatePicker::SetMinDate");
        date_picker
            .SetMaxDate(to_winrt_datetime(*config.range.end()))
            .expect("CalendarDatePicker::SetMaxDate");

        let mut guards = Vec::new();
        let mut revokers = Vec::new();

        let value = config.value.clone();
        revokers.push(
            date_picker
                .DateChanged(move |_sender, args| {
                    let args = args.ok().expect("DateChanged args");
                    let new_date = args.NewDate().expect("DateChangedEventArgs::NewDate");
                    let current = value.snapshot();
                    let time = current.time();
                    let date = from_winrt_datetime(new_date).date();
                    value.set(date.at(
                        time.hour(),
                        time.minute(),
                        time.second(),
                        time.subsec_nanosecond(),
                    ));
                })
                .expect("CalendarDatePicker::DateChanged"),
        );

        let queue = renderer.executor().queue().clone();
        let weak = date_picker.downgrade().expect("weak ref");
        guards.push(
            subscribe_then_get(&config.value, move |ctx| {
                let value = ctx.into_value();
                let weak = weak.clone();
                let queue = queue.clone();
                enqueue_on_ui_thread(&queue, move || {
                    if let Some(picker) = weak.upgrade() {
                        let winrt = to_winrt_datetime(value);
                        if picker.Date().expect("Date") != winrt {
                            picker.SetDate(Some(winrt)).expect("SetDate");
                        }
                    }
                });
            })
            .1,
        );

        crate::util::children(&row)
            .Append(&date_picker.cast::<UIElement>().expect("UIElement"))
            .expect("Append date picker");

        if wants_time {
            let time_picker = TimePicker::new().expect("TimePicker::new");
            let current = config.value.snapshot().time();
            time_picker
                .SetSelectedTime(Some(windows_time::TimeSpan {
                    duration: (i64::from(current.hour()) * 3600
                        + i64::from(current.minute()) * 60
                        + i64::from(current.second()))
                        * 10_000_000,
                }))
                .expect("TimePicker::SetSelectedTime");

            let value = config.value.clone();
            revokers.push(
                time_picker
                    .SelectedTimeChanged(move |_sender, args| {
                        let args = args.ok().expect("SelectedTimeChanged args");
                        let new_time = args.NewTime().expect("NewTime");
                        let total_seconds = new_time.duration / 10_000_000;
                        let hour = i8::try_from(total_seconds / 3600).expect("hour fits i8");
                        let minute =
                            i8::try_from((total_seconds % 3600) / 60).expect("minute fits i8");
                        let second = i8::try_from(total_seconds % 60).expect("second fits i8");
                        let current = value.snapshot();
                        value.set(current.date().at(hour, minute, second, 0));
                    })
                    .expect("TimePicker::SelectedTimeChanged"),
            );

            let queue = renderer.executor().queue().clone();
            let weak = time_picker.downgrade().expect("weak ref");
            guards.push(
                subscribe_then_get(&config.value, move |ctx| {
                    let value = ctx.into_value();
                    let weak = weak.clone();
                    let queue = queue.clone();
                    enqueue_on_ui_thread(&queue, move || {
                        if let Some(picker) = weak.upgrade() {
                            let time = value.time();
                            let span = windows_time::TimeSpan {
                                duration: (i64::from(time.hour()) * 3600
                                    + i64::from(time.minute()) * 60
                                    + i64::from(time.second()))
                                    * 10_000_000,
                            };
                            if picker.SelectedTime().expect("SelectedTime") != span {
                                picker.SetSelectedTime(Some(span)).expect("SetSelectedTime");
                            }
                        }
                    });
                })
                .1,
            );

            crate::util::children(&row)
                .Append(&time_picker.cast::<UIElement>().expect("UIElement"))
                .expect("Append time picker");
        }

        crate::util::children(&panel)
            .Append(&row.cast::<UIElement>().expect("UIElement"))
            .expect("Append row");

        let element = framework(&panel.cast().expect("UIElement"));
        store_watcher_guards(&element, guards);
        for revoker in revokers {
            store_event_revoker(&element, revoker);
        }
        panel.cast().expect("StackPanel is a UIElement")
    }
}

impl WinUiComponent for Native<MultiDatePickerConfig> {
    /// Renders a multi-select `CalendarView`.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let panel = StackPanel::new().expect("StackPanel::new");
        panel
            .SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        panel.SetSpacing(6.0).expect("StackPanel::SetSpacing");

        let label = renderer.render(config.label, env);
        crate::util::children(&panel)
            .Append(&label)
            .expect("Append label");

        let calendar = CalendarView::new().expect("CalendarView::new");
        calendar
            .SetSelectionMode(CalendarViewSelectionMode::Multiple)
            .expect("CalendarView::SetSelectionMode");
        calendar
            .SetMinDate(to_winrt_date(*config.range.start()))
            .expect("CalendarView::SetMinDate");
        calendar
            .SetMaxDate(to_winrt_date(*config.range.end()))
            .expect("CalendarView::SetMaxDate");

        let selected: windows_collections::IVector<windows_time::DateTime> = crate::util::vector(
            &calendar
                .SelectedDates()
                .expect("CalendarView::SelectedDates"),
        );
        for date in config.value.snapshot() {
            selected
                .Append(to_winrt_date(date))
                .expect("IVector<DateTime>::Append");
        }

        let mut guards = Vec::new();
        let mut revokers = Vec::new();

        let value = config.value.clone();
        let selected_for_event = selected.clone();
        revokers.push(
            calendar
                .SelectedDatesChanged(move |_sender, _args| {
                    let count = selected_for_event.Size().expect("IVector::Size");
                    let mut dates = Vec::with_capacity(count as usize);
                    for index in 0..count {
                        let winrt = selected_for_event.GetAt(index).expect("IVector::GetAt");
                        dates.push(from_winrt_date(winrt));
                    }
                    value.set(dates);
                })
                .expect("CalendarView::SelectedDatesChanged"),
        );

        let queue = renderer.executor().queue().clone();
        let weak = calendar.downgrade().expect("weak ref");
        guards.push(
            subscribe_then_get(&config.value, move |ctx| {
                let dates = ctx.into_value();
                let weak = weak.clone();
                let queue = queue.clone();
                enqueue_on_ui_thread(&queue, move || {
                    if let Some(calendar) = weak.upgrade() {
                        let selected: windows_collections::IVector<windows_time::DateTime> =
                            crate::util::vector(&calendar.SelectedDates().expect("SelectedDates"));
                        selected.Clear().expect("IVector::Clear");
                        for date in dates {
                            selected
                                .Append(to_winrt_date(date))
                                .expect("IVector::Append");
                        }
                    }
                });
            })
            .1,
        );

        crate::util::children(&panel)
            .Append(&calendar.cast::<UIElement>().expect("UIElement"))
            .expect("Append calendar");

        let element = framework(&panel.cast().expect("UIElement"));
        store_watcher_guards(&element, guards);
        for revoker in revokers {
            store_event_revoker(&element, revoker);
        }
        panel.cast().expect("StackPanel is a UIElement")
    }
}

impl WinUiComponent for Native<ColorPickerConfig> {
    /// Renders a `DropDownButton` whose flyout hosts a `ColorPicker`.
    #[allow(clippy::too_many_lines)] // flat registration/wiring code
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();

        let panel = StackPanel::new().expect("StackPanel::new");
        panel
            .SetOrientation(Orientation::Vertical)
            .expect("StackPanel::SetOrientation");
        panel.SetSpacing(6.0).expect("StackPanel::SetSpacing");

        let label = renderer.render(config.label, env);
        crate::util::children(&panel)
            .Append(&label)
            .expect("Append label");

        let picker = ColorPicker::new().expect("ColorPicker::new");
        picker
            .SetIsAlphaEnabled(config.support_alpha)
            .expect("ColorPicker::SetIsAlphaEnabled");
        let initial = config.value.snapshot().resolve(env).snapshot();
        picker
            .SetColor(resolved_color_to_winui(&initial))
            .expect("ColorPicker::SetColor");

        let flyout = Flyout::new().expect("Flyout::new");
        flyout
            .SetContent(&picker.cast::<UIElement>().expect("UIElement"))
            .expect("Flyout::SetContent");

        let button = DropDownButton::new().expect("DropDownButton::new");
        button
            .cast::<IButton>()
            .expect("DropDownButton is an IButton")
            .SetFlyout(&flyout)
            .expect("IButton::SetFlyout");
        let swatch = Border::new().expect("Border::new");
        let swatch_fe = framework(&swatch.cast::<UIElement>().expect("UIElement"));
        swatch_fe
            .SetWidth(40.0)
            .expect("FrameworkElement::SetWidth");
        swatch_fe
            .SetHeight(24.0)
            .expect("FrameworkElement::SetHeight");
        swatch
            .SetBackground(&crate::util::solid_brush(&initial).expect("SolidColorBrush"))
            .expect("Border::SetBackground");
        swatch
            .SetCornerRadius(CornerRadius {
                top_left: 4.0,
                top_right: 4.0,
                bottom_right: 4.0,
                bottom_left: 4.0,
            })
            .expect("Border::SetCornerRadius");
        button
            .cast::<ContentControl>()
            .expect("DropDownButton is a ContentControl")
            .SetContent(&swatch.cast::<UIElement>().expect("UIElement"))
            .expect("ContentControl::SetContent");

        let mut guards = Vec::new();
        let mut revokers = Vec::new();

        let value = config.value.clone();
        revokers.push(
            picker
                .ColorChanged(move |_sender, args| {
                    let args = args.ok().expect("ColorChanged args");
                    let color = args.NewColor().expect("ColorChangedEventArgs::NewColor");
                    value.set(
                        waterui_graphics::color::Color::srgb_f32(
                            f32::from(color.r) / 255.0,
                            f32::from(color.g) / 255.0,
                            f32::from(color.b) / 255.0,
                        )
                        .with_opacity(f32::from(color.a) / 255.0),
                    );
                })
                .expect("ColorPicker::ColorChanged"),
        );

        let queue = renderer.executor().queue().clone();
        let weak_picker = picker.downgrade().expect("weak ref");
        let weak_swatch = swatch.downgrade().expect("weak ref");
        let env_for_watch = env.clone();
        guards.push(
            subscribe_then_get(&config.value, move |ctx| {
                let color = ctx.into_value().resolve(&env_for_watch).snapshot();
                let weak_picker = weak_picker.clone();
                let weak_swatch = weak_swatch.clone();
                let queue = queue.clone();
                enqueue_on_ui_thread(&queue, move || {
                    let winui = resolved_color_to_winui(&color);
                    if let Some(picker) = weak_picker.upgrade()
                        && picker.Color().expect("Color") != winui
                    {
                        picker.SetColor(winui).expect("SetColor");
                    }
                    if let Some(swatch) = weak_swatch.upgrade() {
                        swatch
                            .SetBackground(
                                &crate::util::solid_brush(&color).expect("SolidColorBrush"),
                            )
                            .expect("Border::SetBackground");
                    }
                });
            })
            .1,
        );

        crate::util::children(&panel)
            .Append(&button.cast::<UIElement>().expect("UIElement"))
            .expect("Append button");

        let element = framework(&panel.cast().expect("UIElement"));
        store_watcher_guards(&element, guards);
        for revoker in revokers {
            store_event_revoker(&element, revoker);
        }
        panel.cast().expect("StackPanel is a UIElement")
    }
}
