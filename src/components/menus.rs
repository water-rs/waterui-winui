//! Menu realization: `ResolvedMenu` as a `DropDownButton` + `MenuFlyout`, the
//! app's menu bar as a `MenuBar`, and every command's `Shortcut` as a
//! `KeyboardAccelerator` on its row.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use nami::{Computed, Signal};
use waterui::app::Quit;
use waterui_controls::menu::{
    CommandExt, ResolvedMenu, ResolvedMenuItem, ResolvedNestedMenu, Shortcut,
};
use waterui_core::{Environment, Native};
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::{UiThread, enqueue_on_ui_thread};
use crate::renderer::WinUiRenderer;
use crate::util::{framework, store_event_revoker, store_watcher_guard};

impl WinUiComponent for Native<ResolvedMenu> {
    /// Renders a `DropDownButton` that opens a `MenuFlyout` of resolved items.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let menu = self.into_inner();

        let button = DropDownButton::new().expect("DropDownButton::new");
        let label = renderer.render_any(menu.label, env);
        button
            .cast::<ContentControl>()
            .expect("DropDownButton is a ContentControl")
            .SetContent(&label)
            .expect("ContentControl::SetContent");

        let flyout = MenuFlyout::new().expect("MenuFlyout::new");
        rebuild_flyout(
            &flyout,
            &menu.items.snapshot(),
            env,
            renderer.ui_thread(),
            &ShortcutArming::WhileMounted,
        );

        button
            .cast::<IButton>()
            .expect("DropDownButton is a Button")
            .SetFlyout(&flyout)
            .expect("IButton::SetFlyout");

        let element: UIElement = button.cast().expect("DropDownButton is a UIElement");
        watch_items(
            &menu.items,
            &flyout,
            &element,
            env,
            renderer.ui_thread(),
            |flyout, items, env, ui| {
                rebuild_flyout(flyout, items, env, ui, &ShortcutArming::WhileMounted);
            },
        );
        element
    }
}

/// Replaces a `MenuFlyout`'s items with freshly built entries.
pub(crate) fn rebuild_flyout(
    flyout: &MenuFlyout,
    items: &[ResolvedMenuItem],
    env: &Environment,
    ui: &UiThread,
    arming: &ShortcutArming,
) {
    if let ShortcutArming::WhileOpen(accelerators) = arming {
        // A rebuild drops the previous rows; their chords leave with them.
        accelerators.reset();
    }
    let collection =
        crate::util::vector::<_, MenuFlyoutItemBase>(&flyout.Items().expect("MenuFlyout::Items"));
    fill_rows(&collection, items, env, ui, arming);
    if let ShortcutArming::WhileOpen(accelerators) = arming {
        // The flyout raises `Opened` only once per opening, so rows built
        // while it is already open arm at once.
        accelerators.arm();
    }
}

/// Clears `rows` and appends a freshly built `MenuFlyoutItemBase` per item.
fn fill_rows(
    rows: &windows_collections::IVector<MenuFlyoutItemBase>,
    items: &[ResolvedMenuItem],
    env: &Environment,
    ui: &UiThread,
    arming: &ShortcutArming,
) {
    rows.Clear().expect("IVector::Clear");
    for item in items {
        rows.Append(&build_menu_item(item, env, ui, arming))
            .expect("IVector::Append");
    }
}

/// When a row's `Shortcut` chord fires, per the Hydrolysis contract: a
/// mounted menu's rows (the menu bar, a `Menu`'s flyout) answer while
/// mounted, a `.context_menu`'s rows only while its flyout is open.
#[derive(Clone)]
pub(crate) enum ShortcutArming {
    /// Append each `KeyboardAccelerator` to its row at build.
    WhileMounted,
    /// Park each `KeyboardAccelerator`; the flyout's `Opened` arms them and
    /// `Closed` clears them.
    WhileOpen(Rc<ContextFlyoutAccelerators>),
}

/// The chords a context flyout parks, live only while the flyout reports
/// open. `register_context_menu` opens and closes them on the flyout's
/// `Opened`/`Closed`; each rebuild parks the new rows' chords here.
#[derive(Default)]
pub(crate) struct ContextFlyoutAccelerators {
    /// `true` between the flyout's `Opened` and `Closed`.
    open: Cell<bool>,
    /// The current rows' accelerators — armed while `open`.
    parked: RefCell<Vec<(UIElement, KeyboardAccelerator)>>,
}

impl ContextFlyoutAccelerators {
    /// The flyout reports open: every parked chord goes live.
    pub(crate) fn opened(&self) {
        self.open.set(true);
        self.arm();
    }

    /// The flyout reports closed: the chords die with it.
    pub(crate) fn closed(&self) {
        self.open.set(false);
        self.disarm();
    }

    /// Rebuild entry: the previous rows' chords leave, armed or not; the
    /// new rows park theirs and [`Self::arm`] takes it from there.
    fn reset(&self) {
        self.disarm();
        self.parked.borrow_mut().clear();
    }

    /// Parks a built row's accelerator for `Opened`.
    fn park(&self, element: UIElement, accelerator: KeyboardAccelerator) {
        self.parked.borrow_mut().push((element, accelerator));
    }

    /// Arms every parked chord while the menu is open.
    fn arm(&self) {
        if !self.open.get() {
            return;
        }
        for (element, accelerator) in &*self.parked.borrow() {
            crate::util::vector::<_, KeyboardAccelerator>(
                &element
                    .KeyboardAccelerators()
                    .expect("UIElement::KeyboardAccelerators"),
            )
            .Append(accelerator)
            .expect("IVector::Append");
        }
    }

    /// Clears every parked chord that is armed.
    fn disarm(&self) {
        for (element, accelerator) in &*self.parked.borrow() {
            let accelerators = crate::util::vector::<_, KeyboardAccelerator>(
                &element
                    .KeyboardAccelerators()
                    .expect("UIElement::KeyboardAccelerators"),
            );
            let mut index = 0;
            if accelerators
                .IndexOf(accelerator, &mut index)
                .expect("IVector::IndexOf")
            {
                accelerators.RemoveAt(index).expect("IVector::RemoveAt");
            }
        }
    }
}

/// Builds one `MenuFlyoutItemBase` from a resolved menu item.
fn build_menu_item(
    item: &ResolvedMenuItem,
    env: &Environment,
    ui: &UiThread,
    arming: &ShortcutArming,
) -> MenuFlyoutItemBase {
    match item {
        ResolvedMenuItem::Command(command) => {
            let entry = MenuFlyoutItem::new().expect("MenuFlyoutItem::new");
            let text = command.label.content.snapshot().to_plain().to_string();
            entry
                .SetText(text.as_str())
                .expect("MenuFlyoutItem::SetText");
            entry
                .cast::<Control>()
                .expect("MenuFlyoutItem is a Control")
                .SetIsEnabled(!command.disabled.snapshot())
                .expect("Control::SetIsEnabled");

            if let Some(icon) = &command.icon
                && let Some(symbol) = super::graphics::segoe_symbol_for(icon.name.as_str())
            {
                let native = SymbolIcon::new().expect("SymbolIcon::new");
                native.SetSymbol(symbol).expect("SymbolIcon::SetSymbol");
                entry
                    .SetIcon(&native.cast::<IconElement>().expect("IconElement"))
                    .expect("MenuFlyoutItem::SetIcon");
            }

            if let Some(shortcut) = &command.shortcut {
                let action = command.action.clone();
                let env = env.clone();
                arm_shortcut(&entry, shortcut, arming, move || action.call(&env));
            }

            let action = command.action.clone();
            let env = env.clone();
            let revoker = entry
                .Click(move |_sender, _args| {
                    action.call(&env);
                })
                .expect("MenuFlyoutItem::Click");
            store_event_revoker(&framework(&entry.cast().expect("UIElement")), revoker);

            let weak = entry.downgrade().expect("weak ref");
            let queue_for_watch = ui.queue().clone();
            let disabled_guard = command.disabled.watch(move |ctx| {
                let disabled = ctx.into_value();
                let weak = weak.clone();
                let queue = queue_for_watch.clone();
                enqueue_on_ui_thread(&queue, move || {
                    if let Some(entry) = weak.upgrade() {
                        entry
                            .cast::<Control>()
                            .expect("MenuFlyoutItem is a Control")
                            .SetIsEnabled(!disabled)
                            .expect("SetIsEnabled");
                    }
                });
            });
            store_watcher_guard(
                &framework(&entry.cast().expect("UIElement")),
                disabled_guard,
            );

            entry
                .cast()
                .expect("MenuFlyoutItem is a MenuFlyoutItemBase")
        }
        ResolvedMenuItem::Divider => MenuFlyoutSeparator::new()
            .expect("MenuFlyoutSeparator::new")
            .cast()
            .expect("MenuFlyoutSeparator is a MenuFlyoutItemBase"),
        ResolvedMenuItem::Menu(nested) => {
            let entry = MenuFlyoutSubItem::new().expect("MenuFlyoutSubItem::new");
            let text = nested.label.content.snapshot().to_plain().to_string();
            entry
                .SetText(text.as_str())
                .expect("MenuFlyoutSubItem::SetText");
            let items = crate::util::vector::<_, MenuFlyoutItemBase>(
                &entry.Items().expect("MenuFlyoutSubItem::Items"),
            );
            for child in nested.items.snapshot() {
                items
                    .Append(&build_menu_item(&child, env, ui, arming))
                    .expect("IVector::Append");
            }
            entry
                .cast()
                .expect("MenuFlyoutSubItem is a MenuFlyoutItemBase")
        }
        ResolvedMenuItem::Quit => {
            let quit = env
                .get::<Quit>()
                .expect("Quit is installed before rendering")
                .clone();
            // The platform quit command — hydrolysis's `quit_command`
            // spells it the same way for Windows.
            let command = "Exit"
                .action(move || quit.request())
                .shortcut(Shortcut::new("q").command())
                .resolve(env);
            build_menu_item(&ResolvedMenuItem::Command(command), env, ui, arming)
        }
    }
}

/// Arms `shortcut` as `entry`'s `KeyboardAccelerator` under `arming`'s
/// contract. `WinUI` resolves a menu row's accelerators through the XAML
/// tree of the window holding the menu, open or not, so a mounted chord is
/// scoped to that window; a `.context_menu` row instead parks its chord for
/// the flyout's `Opened`/`Closed`, keeping it dead while the menu is
/// closed. When two rows declare the same chord `WinUI` answers the first
/// in the window's visual-tree order — the menu bar wins — unlike
/// Hydrolysis's most-recent-source rule. Handling `Invoked` runs the
/// command once instead of also raising the row's `Click`. A shortcut on a
/// named key, or on a character the layout types only with modifiers held,
/// arms nothing and only shows its hint.
fn arm_shortcut(
    entry: &MenuFlyoutItem,
    shortcut: &Shortcut,
    arming: &ShortcutArming,
    on_invoke: impl Fn() + 'static,
) {
    let chord = accelerator_chord(shortcut);
    if chord.is_none() || matches!(arming, ShortcutArming::WhileOpen(_)) {
        // Named keys such as Delete or F5 have no chord WaterUI dispatches
        // yet (water-rs/waterui#2038), and a character such as `!` needs a
        // Shift the shortcut does not declare: like Hydrolysis, the row
        // shows the hint. A parked chord gets the same treatment — the
        // accelerator is not on the row while the menu is closed, so
        // `WinUI` would draw no text without the override.
        entry
            .SetKeyboardAcceleratorTextOverride(hint_text(shortcut).as_str())
            .expect("MenuFlyoutItem::SetKeyboardAcceleratorTextOverride");
    }
    let Some((key, modifiers)) = chord else {
        return;
    };
    let accelerator = KeyboardAccelerator::new().expect("KeyboardAccelerator::new");
    accelerator
        .SetKey(key)
        .expect("KeyboardAccelerator::SetKey");
    accelerator
        .SetModifiers(modifiers)
        .expect("KeyboardAccelerator::SetModifiers");
    let revoker = accelerator
        .Invoked(move |_sender, args| {
            on_invoke();
            args.ok()
                .expect("KeyboardAccelerator::Invoked carries its args")
                .SetHandled(true)
                .expect("KeyboardAcceleratorInvokedEventArgs::SetHandled");
        })
        .expect("KeyboardAccelerator::Invoked");
    let element: UIElement = entry.cast().expect("MenuFlyoutItem is a UIElement");
    match arming {
        ShortcutArming::WhileMounted => {
            store_event_revoker(&framework(&element), revoker);
            crate::util::vector::<_, KeyboardAccelerator>(
                &element
                    .KeyboardAccelerators()
                    .expect("UIElement::KeyboardAccelerators"),
            )
            .Append(&accelerator)
            .expect("IVector::Append");
        }
        ShortcutArming::WhileOpen(accelerators) => {
            // The handler captures the action, not the row, so the
            // accelerator owns its `Invoked` registration outright and the
            // parked chord lives with the flyout's state.
            revoker.into_token();
            accelerators.park(element, accelerator);
        }
    }
}

/// The `KeyboardAccelerator` key and modifiers for `shortcut`, matching
/// Hydrolysis off macOS: the key is compared case insensitively and the
/// command modifier is Control. `None` for a key that is not a single
/// character, since Hydrolysis dispatches single-character presses only, and
/// for a character the active keyboard layout types only with modifiers held
/// (`!` is Shift+1 on US), whose key alone is a different character.
///
/// # Panics
///
/// When the active keyboard layout cannot type the character at all.
fn accelerator_chord(shortcut: &Shortcut) -> Option<(VirtualKey, VirtualKeyModifiers)> {
    let key = shortcut.key.to_lowercase();
    let mut chars = key.chars();
    let (Some(character), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let mut units = [0; 2];
    let &mut [unit] = character.encode_utf16(&mut units) else {
        panic!("shortcut key {character:?} cannot be typed on the active keyboard layout");
    };
    // SAFETY: a lookup in the calling thread's keyboard layout.
    let scan = unsafe { VkKeyScanW(unit) };
    // VkKeyScanW reports a character no key types as -1 in both bytes.
    assert!(
        scan != -1,
        "shortcut key {character:?} cannot be typed on the active keyboard layout"
    );
    let [virtual_key, shift_state] = scan.to_le_bytes();
    if shift_state != 0 {
        return None;
    }
    let modifiers = shortcut.modifiers;
    let mut chord = VirtualKeyModifiers::None.0;
    if modifiers.control() || modifiers.command() {
        chord |= VirtualKeyModifiers::Control.0;
    }
    if modifiers.option() {
        chord |= VirtualKeyModifiers::Menu.0;
    }
    if modifiers.shift() {
        chord |= VirtualKeyModifiers::Shift.0;
    }
    Some((
        VirtualKey(i32::from(virtual_key)),
        VirtualKeyModifiers(chord),
    ))
}

/// The accelerator text Hydrolysis draws for `shortcut` off macOS, shown on
/// a row whose key arms no `KeyboardAccelerator`.
fn hint_text(shortcut: &Shortcut) -> String {
    let modifiers = shortcut.modifiers;
    let key = shortcut.key.to_uppercase();
    [
        (modifiers.control() || modifiers.command(), "Ctrl"),
        (modifiers.option(), "Alt"),
        (modifiers.shift(), "Shift"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .chain([key.as_str()])
    .collect::<Vec<_>>()
    .join("+")
}

/// Watches `items` and runs `rebuild` for `target` on the UI thread each
/// time the list changes. `target` is captured weakly — the guard lives on
/// `owner`'s attachment bag, which can outlive `target` — so a dead target
/// simply stops being rebuilt.
fn watch_items<T: Interface + 'static>(
    items: &Computed<Vec<ResolvedMenuItem>>,
    target: &T,
    owner: &UIElement,
    env: &Environment,
    ui: &UiThread,
    rebuild: impl Fn(&T, &[ResolvedMenuItem], &Environment, &UiThread) + 'static,
) {
    let weak = target.downgrade().expect("menu target supports weak refs");
    let env_for_watch = env.clone();
    let ui_for_watch = ui.clone();
    let rebuild = Rc::new(rebuild);
    let guard = items.watch(move |ctx| {
        let items = ctx.into_value();
        let weak = weak.clone();
        let env = env_for_watch.clone();
        let ui = ui_for_watch.clone();
        let rebuild = rebuild.clone();
        ui_for_watch.enqueue(move || {
            if let Some(target) = weak.upgrade() {
                rebuild(&target, &items, &env, &ui);
            }
        });
    });
    store_watcher_guard(&framework(owner), guard);
}

/// The app's menu bar: a `MenuBar` of one `MenuBarItem` per top-level menu,
/// rebuilt when the menus change and collapsed while there are none.
pub(crate) fn build_menu_bar(
    menus: &Computed<Vec<ResolvedMenuItem>>,
    env: &Environment,
    ui: &UiThread,
) -> UIElement {
    let bar = MenuBar::new().expect("MenuBar::new");
    rebuild_menu_bar(&bar, &menus.snapshot(), env, ui);

    let element: UIElement = bar.cast().expect("MenuBar is a UIElement");
    watch_items(menus, &bar, &element, env, ui, rebuild_menu_bar);
    element
}

/// Replaces a `MenuBar`'s menus with freshly built ones.
fn rebuild_menu_bar(bar: &MenuBar, menus: &[ResolvedMenuItem], env: &Environment, ui: &UiThread) {
    let entries = crate::util::vector::<_, MenuBarItem>(&bar.Items().expect("MenuBar::Items"));
    entries.Clear().expect("IVector::Clear");
    for menu in menus {
        let ResolvedMenuItem::Menu(menu) = menu else {
            unreachable!("resolve_menu_bar_items resolves every top-level entry to a menu");
        };
        entries
            .Append(&build_menu_bar_item(menu, env, ui))
            .expect("IVector::Append");
    }
    bar.cast::<UIElement>()
        .expect("MenuBar is a UIElement")
        .SetVisibility(if menus.is_empty() {
            Visibility::Collapsed
        } else {
            Visibility::Visible
        })
        .expect("UIElement::SetVisibility");
}

/// One top-level menu of the bar. A nested menu's items can change without
/// the bar's list re-emitting, so its rows rebuild on their own.
fn build_menu_bar_item(menu: &ResolvedNestedMenu, env: &Environment, ui: &UiThread) -> MenuBarItem {
    let entry = MenuBarItem::new().expect("MenuBarItem::new");
    let title = menu.label.content.snapshot().to_plain().to_string();
    entry
        .SetTitle(title.as_str())
        .expect("MenuBarItem::SetTitle");
    fill_menu_bar_item(&entry, &menu.items.snapshot(), env, ui);

    let element: UIElement = entry.cast().expect("MenuBarItem is a UIElement");
    watch_items(&menu.items, &entry, &element, env, ui, fill_menu_bar_item);
    entry
}

/// Replaces a `MenuBarItem`'s rows with freshly built entries.
fn fill_menu_bar_item(
    entry: &MenuBarItem,
    items: &[ResolvedMenuItem],
    env: &Environment,
    ui: &UiThread,
) {
    let rows =
        crate::util::vector::<_, MenuFlyoutItemBase>(&entry.Items().expect("MenuBarItem::Items"));
    fill_rows(&rows, items, env, ui, &ShortcutArming::WhileMounted);
}

#[cfg(test)]
mod tests {
    use waterui_controls::menu::Shortcut;

    use super::{accelerator_chord, hint_text};
    use crate::bindings::{VirtualKey, VirtualKeyModifiers};

    #[test]
    fn the_command_modifier_is_control() {
        assert_eq!(
            accelerator_chord(&Shortcut::new("q").command()),
            Some((VirtualKey::Q, VirtualKeyModifiers::Control))
        );
    }

    #[test]
    fn every_modifier_maps_and_the_key_ignores_case() {
        assert_eq!(
            accelerator_chord(&Shortcut::new("S").control().option().shift()),
            Some((
                VirtualKey::S,
                VirtualKeyModifiers(
                    VirtualKeyModifiers::Control.0
                        | VirtualKeyModifiers::Menu.0
                        | VirtualKeyModifiers::Shift.0
                )
            ))
        );
    }

    #[test]
    fn named_keys_arm_nothing_and_show_their_hint() {
        for key in ["Delete", "F5"] {
            assert_eq!(accelerator_chord(&Shortcut::new(key).command()), None);
        }
        assert_eq!(
            hint_text(&Shortcut::new("Delete").command().shift()),
            "Ctrl+Shift+DELETE"
        );
        assert_eq!(hint_text(&Shortcut::new("F5")), "F5");
    }

    /// `!` is Shift+1 on the US layout the CI runners use.
    #[test]
    fn shift_characters_arm_nothing_and_show_their_hint() {
        assert_eq!(accelerator_chord(&Shortcut::new("!").command()), None);
        assert_eq!(hint_text(&Shortcut::new("!").command()), "Ctrl+!");
    }

    #[test]
    #[should_panic(expected = "shortcut key '☃' cannot be typed on the active keyboard layout")]
    fn characters_the_layout_cannot_type_panic() {
        accelerator_chord(&Shortcut::new("☃"));
    }

    /// A character needing a surrogate pair has no key to arm at all.
    #[test]
    #[should_panic(expected = "shortcut key '😀' cannot be typed")]
    fn astral_characters_panic() {
        accelerator_chord(&Shortcut::new("😀"));
    }
}
