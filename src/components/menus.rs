//! Menu realization: `ResolvedMenu` as a `DropDownButton` + `MenuFlyout`.

use nami::Signal;
use waterui_controls::menu::{ResolvedMenu, ResolvedMenuItem};
use waterui_core::{Environment, Native};
use windows_core::Interface;

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
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
        rebuild_flyout(&flyout, &menu.items.get(), env, renderer.executor().queue());

        let queue = renderer.executor().queue().clone();
        let weak = flyout.downgrade().expect("MenuFlyout supports weak refs");
        let env_for_watch = env.clone();
        let items_guard = menu.items.watch(move |ctx| {
            let items = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            let env = env_for_watch.clone();
            let queue_in = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(flyout) = weak.upgrade() {
                    rebuild_flyout(&flyout, &items, &env, &queue_in);
                }
            });
        });

        button
            .cast::<IButton>()
            .expect("DropDownButton is a Button")
            .SetFlyout(&flyout)
            .expect("IButton::SetFlyout");

        let element: FrameworkElement = button.cast().expect("FrameworkElement");
        store_watcher_guard(&element, items_guard);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

/// Replaces a `MenuFlyout`'s items with freshly built entries.
pub(crate) fn rebuild_flyout(
    flyout: &MenuFlyout,
    items: &[ResolvedMenuItem],
    env: &Environment,
    queue: &DispatcherQueue,
) {
    let collection =
        crate::util::vector::<_, MenuFlyoutItemBase>(&flyout.Items().expect("MenuFlyout::Items"));
    collection.Clear().expect("IVector::Clear");
    for item in items {
        collection
            .Append(&build_menu_item(item, env, queue))
            .expect("IVector::Append");
    }
}

/// Builds one `MenuFlyoutItemBase` from a resolved menu item.
fn build_menu_item(
    item: &ResolvedMenuItem,
    env: &Environment,
    queue: &DispatcherQueue,
) -> MenuFlyoutItemBase {
    match item {
        ResolvedMenuItem::Command(command) => {
            let entry = MenuFlyoutItem::new().expect("MenuFlyoutItem::new");
            let text = command.label.content.get().to_plain().to_string();
            entry
                .SetText(text.as_str())
                .expect("MenuFlyoutItem::SetText");
            entry
                .cast::<Control>()
                .expect("MenuFlyoutItem is a Control")
                .SetIsEnabled(!command.disabled.get())
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

            let action = command.action.clone();
            let env = env.clone();
            let revoker = entry
                .Click(move |_sender, _args| {
                    action.call(&env);
                })
                .expect("MenuFlyoutItem::Click");
            store_event_revoker(&framework(&entry.cast().expect("UIElement")), revoker);

            let weak = entry.downgrade().expect("weak ref");
            let queue_for_watch = queue.clone();
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
            let text = nested.label.content.get().to_plain().to_string();
            entry
                .SetText(text.as_str())
                .expect("MenuFlyoutSubItem::SetText");
            let items = crate::util::vector::<_, MenuFlyoutItemBase>(
                &entry.Items().expect("MenuFlyoutSubItem::Items"),
            );
            for child in nested.items.get() {
                items
                    .Append(&build_menu_item(&child, env, queue))
                    .expect("IVector::Append");
            }
            entry
                .cast()
                .expect("MenuFlyoutSubItem is a MenuFlyoutItemBase")
        }
    }
}
