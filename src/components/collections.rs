//! Collection views: `List` and `TabsLayout`.
#![allow(clippy::inline_always, clippy::ref_as_ptr)] // generated `implement` macro items

use std::rc::Rc;

use nami::Signal;
use waterui::component::list::ListConfig;
use waterui_core::id::Id;
use waterui_core::views::Views;
use waterui_core::{Environment, Native};
use waterui_navigation::tab::{NativeTabStyle, TabIcon, TabsLayout};
use windows_core::{Interface, Ref, implement};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{framework, store_event_revoker, store_watcher_guards};

/// `IElementFactory` that realizes list rows on demand.
///
/// The repeater's items source is a plain index vector; `GetElement` is
/// invoked only for rows inside the viewport plus the cache margin, so a
/// 100,000-row list materializes a handful of elements instead of blocking
/// the UI thread on eager realization.
#[implement(IElementFactory)]
struct RowFactory {
    contents: waterui_core::views::SharedAnyViews<waterui::component::list::ListItem>,
    env: Environment,
}

impl IElementFactory_Impl for RowFactory_Impl {
    fn GetElement(&self, args: Ref<ElementFactoryGetArgs>) -> windows_core::Result<UIElement> {
        let index = usize::try_from(
            args.ok()?
                .Data()?
                .cast::<windows_reference::IReference<i32>>()?
                .Value()?,
        )
        .expect("row indices are non-negative");
        let item = self
            .contents
            .get_view(index)
            .expect("ItemsRepeater only requests indices below the items-source count");
        let mut renderer =
            WinUiRenderer::for_current_thread().expect("row rendering requires the UI thread");
        Ok(renderer.render_any(item.content, &self.env))
    }

    fn RecycleElement(&self, _args: Ref<ElementFactoryRecycleArgs>) -> windows_core::Result<()> {
        // Rows are arbitrary WaterUI view trees, not rebindable containers;
        // recycling is optional, so cleared elements are simply discarded.
        Ok(())
    }
}

/// Builds the repeater's items source: each item is the row index boxed as an
/// `IInspectable` (`ItemsRepeater` only accepts `IVector<IInspectable>`).
fn index_vector(count: usize) -> windows_core::IInspectable {
    let items: Vec<Option<windows_core::IInspectable>> = (0..i32::try_from(count)
        .expect("row count fits i32"))
        .map(|index| Some(PropertyValue::CreateInt32(index).expect("PropertyValue::CreateInt32")))
        .collect();
    windows_collections::IVector::<windows_core::IInspectable>::from(items)
        .cast::<windows_core::IInspectable>()
        .expect("IVector is an IInspectable")
}

impl WinUiComponent for Native<ListConfig> {
    /// Renders a virtualizing `ItemsRepeater` inside a `ScrollViewer`; rows
    /// are realized lazily through `RowFactory`.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let config = self.into_inner();
        let contents = config.contents.clone();
        let env = env.clone();

        let repeater = ItemsRepeater::new().expect("ItemsRepeater::new");
        let factory: IElementFactory = RowFactory {
            contents: contents.clone(),
            env: env.clone(),
        }
        .into();
        repeater
            .SetItemTemplate(
                &factory
                    .cast::<windows_core::IInspectable>()
                    .expect("IElementFactory is an IInspectable"),
            )
            .expect("ItemsRepeater::SetItemTemplate");
        repeater
            .SetItemsSource(&index_vector(contents.len().get()))
            .expect("ItemsRepeater::SetItemsSource");

        // Reactive refresh: a changed collection swaps the items source and
        // the repeater re-virtualizes against the new count.
        let queue = renderer.executor().queue().clone();
        let weak = repeater
            .downgrade()
            .expect("ItemsRepeater supports weak refs");
        let mut guards = vec![contents.watch(.., move |ctx| {
            let count = ctx.value().to_vec().len();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(repeater) = weak.upgrade() {
                    repeater
                        .SetItemsSource(&index_vector(count))
                        .expect("ItemsRepeater::SetItemsSource");
                }
            });
        })];

        // Programmatic scroll-to-index: realize the target row, then ask the
        // ancestor ScrollViewer to bring it into view.
        if let Some(controller) = config.scroll_controller {
            let target = controller.target();
            let queue = renderer.executor().queue().clone();
            let weak = repeater.downgrade().expect("weak ref");
            guards.push(controller.generation().watch(move |_| {
                let index = target.get();
                let weak = weak.clone();
                let queue = queue.clone();
                enqueue_on_ui_thread(&queue, move || {
                    if let Some(repeater) = weak.upgrade() {
                        repeater
                            .GetOrCreateElement(
                                i32::try_from(index).expect("scroll target index fits i32"),
                            )
                            .expect("ItemsRepeater::GetOrCreateElement")
                            .StartBringIntoView()
                            .expect("UIElement::StartBringIntoView");
                    }
                });
            }));
        }

        let viewer = ScrollViewer::new().expect("ScrollViewer::new");
        viewer
            .SetVerticalScrollBarVisibility(ScrollBarVisibility::Auto)
            .expect("ScrollViewer::SetVerticalScrollBarVisibility");
        viewer
            .SetHorizontalScrollBarVisibility(ScrollBarVisibility::Disabled)
            .expect("ScrollViewer::SetHorizontalScrollBarVisibility");
        viewer
            .cast::<ContentControl>()
            .expect("ScrollViewer is a ContentControl")
            .SetContent(&repeater.cast::<UIElement>().expect("UIElement"))
            .expect("ContentControl::SetContent");

        let element = framework(&viewer.cast().expect("UIElement"));
        store_watcher_guards(&element, guards);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}

impl WinUiComponent for TabsLayout {
    /// Renders a `TabView` for the tab-bar style and a `NavigationView` in
    /// `Left` pane mode for the sidebar style.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        match self.style {
            NativeTabStyle::Sidebar => render_sidebar_tabs(self, env, renderer),
            NativeTabStyle::Automatic | NativeTabStyle::TabBar => {
                render_tab_view(self, env, renderer)
            }
        }
    }
}

/// Builds the tab header: optional icon view, the label view, and an optional
/// badge — combined in a horizontal panel when needed.
fn tab_header(
    label_view: waterui_core::AnyView,
    icon: Option<TabIcon>,
    badge: Option<nami::Computed<i32>>,
    env: &Environment,
    renderer: &mut WinUiRenderer,
    queue: &DispatcherQueue,
    guards: &mut Vec<nami::watcher::BoxWatcherGuard>,
) -> UIElement {
    let label = renderer.render_any(label_view, env);

    let icon_element = icon.map(|icon| match icon {
        TabIcon::System(system) => {
            let symbol = super::graphics::segoe_symbol_for(system.name.as_str())
                .expect("tab system icon must have a Segoe Fluent equivalent");
            let icon_element = SymbolIcon::new().expect("SymbolIcon::new");
            icon_element
                .SetSymbol(symbol)
                .expect("SymbolIcon::SetSymbol");
            icon_element.cast::<UIElement>().expect("UIElement")
        }
        TabIcon::View(builder) => renderer.render_any(builder.build(), env),
    });

    let badge_element = badge.map(|badge| {
        let element = InfoBadge::new().expect("InfoBadge::new");
        element.SetValue(badge.get()).expect("InfoBadge::SetValue");
        let weak = element.downgrade().expect("weak ref");
        let queue = queue.clone();
        guards.push(badge.watch(move |ctx| {
            let value = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(badge) = weak.upgrade() {
                    badge.SetValue(value).expect("InfoBadge::SetValue");
                }
            });
        }));
        element.cast::<UIElement>().expect("UIElement")
    });

    if icon_element.is_none() && badge_element.is_none() {
        return label;
    }

    let panel = StackPanel::new().expect("StackPanel::new");
    panel
        .SetOrientation(Orientation::Horizontal)
        .expect("StackPanel::SetOrientation");
    panel.SetSpacing(6.0).expect("StackPanel::SetSpacing");
    let children = crate::util::children(&panel);
    if let Some(icon) = icon_element {
        children.Append(&icon).expect("Append icon");
    }
    children.Append(&label).expect("Append label");
    if let Some(badge) = badge_element {
        children.Append(&badge).expect("Append badge");
    }
    panel.cast().expect("StackPanel is a UIElement")
}

fn render_tab_view(
    layout: TabsLayout,
    env: &Environment,
    renderer: &mut WinUiRenderer,
) -> UIElement {
    let tab_view = TabView::new().expect("TabView::new");
    tab_view
        .SetIsAddTabButtonVisible(false)
        .expect("TabView::SetIsAddTabButtonVisible");

    let ids: Vec<Id> = layout.tabs.iter().map(|tab| tab.id).collect();
    let mut guards = Vec::new();

    let mut items: Vec<windows_core::IInspectable> = Vec::new();
    for tab in layout.tabs {
        let item = TabViewItem::new().expect("TabViewItem::new");
        let queue = renderer.executor().queue().clone();
        let header = tab_header(
            tab.label,
            tab.icon,
            tab.badge,
            env,
            renderer,
            &queue,
            &mut guards,
        );
        item.SetHeader(&header).expect("TabViewItem::SetHeader");
        item.SetIsClosable(false)
            .expect("TabViewItem::SetIsClosable");

        // The tab content is a lazily built `NavigationView`; inside a tab the
        // bar collapses, matching the GTK backend which renders its content.
        let navigation_view = tab.content.build();
        let content = renderer.render_any(navigation_view.content, env);
        item.cast::<ContentControl>()
            .expect("TabViewItem is a ContentControl")
            .SetContent(&content)
            .expect("ContentControl::SetContent");
        item.cast::<Control>()
            .expect("TabViewItem is a Control")
            .SetIsEnabled(tab.enabled.get())
            .expect("Control::SetIsEnabled");
        items.push(
            item.cast::<windows_core::IInspectable>()
                .expect("IInspectable"),
        );
    }

    // Feed the strip through `TabItemsSource`, which binds the inner
    // `ListView`'s `ItemsSource` directly. Items appended to `TabItems` are
    // copied into the `ListView` on load and never materialize in practice.
    let source_vec: Vec<Option<windows_core::IInspectable>> =
        items.iter().cloned().map(Some).collect();
    let source = windows_collections::IVector::<windows_core::IInspectable>::from(source_vec)
        .cast::<windows_core::IInspectable>()
        .expect("IVector is an IInspectable");
    tab_view
        .SetTabItemsSource(&source)
        .expect("TabView::SetTabItemsSource");

    // The default style aligns the `TabView` to the top with only its desired
    // height, which collapses the `*` content row; stretch it to fill.
    tab_view
        .cast::<FrameworkElement>()
        .expect("FrameworkElement")
        .SetVerticalAlignment(VerticalAlignment::Stretch)
        .expect("SetVerticalAlignment");

    // DEBUG: SizeChanged fires during every layout pass — if even this is
    // silent, the event wiring itself is broken.
    let size_revoker = tab_view
        .cast::<FrameworkElement>()
        .expect("FrameworkElement")
        .SizeChanged(|sender, args| {
            let Ok(sender) = sender.ok() else { return };
            let _ = sender.cast::<FrameworkElement>().expect("FrameworkElement");
            let size = args
                .ok()
                .and_then(|a| a.NewSize())
                .unwrap_or_default();
            tracing::info!("TabView SizeChanged: {}x{}", size.width, size.height);
        })
        .expect("SizeChanged");

    let mut loaded_revoker = None;
    if let Some(index) = ids.iter().position(|id| *id == layout.selection.get()) {
        // Selection must go through `SelectedItem`: on an `ItemsSource`-bound
        // list `SelectedIndex` cannot push into the `ListView` before its
        // containers exist, so the item stays unselected and content blank.
        tab_view
            .SetSelectedItem(&items[index])
            .expect("TabView::SetSelectedItem");

        // `TabView` pushes `SelectedItem` into its inner `ListView` only while
        // the list exists, then syncs the property back from the list's
        // current state at `OnListViewLoaded` — clobbering a pre-load
        // selection whose push the not-yet-populated items view ignored.
        // Re-apply once the control is loaded, and once more at low priority
        // in case the items view only materializes after `Loaded`.
        let weak_for_loaded = tab_view.downgrade().expect("weak ref");
        let queue = renderer.executor().queue().clone();
        let selected = items[index].clone();
        loaded_revoker = Some(
            tab_view
                .cast::<FrameworkElement>()
                .expect("FrameworkElement")
                .Loaded(move |_, _| {
                    let Some(tab_view) = weak_for_loaded.upgrade() else {
                        return;
                    };
                    tracing::info!(
                        "TabView Loaded: index={:?} item_ok={} height={:?}",
                        tab_view.SelectedIndex(),
                        tab_view.SelectedItem().is_ok(),
                        tab_view
                            .cast::<FrameworkElement>()
                            .and_then(|fe| fe.ActualHeight()),
                    );
                    tab_view
                        .SetSelectedItem(&selected)
                        .expect("TabView::SetSelectedItem");
                    tracing::info!(
                        "TabView after re-apply: index={:?} item_ok={}",
                        tab_view.SelectedIndex(),
                        tab_view.SelectedItem().is_ok(),
                    );
                    let weak = tab_view.downgrade().expect("weak ref");
                    let selected = selected.clone();
                    queue
                        .TryEnqueueWithPriority(
                            DispatcherQueuePriority::Low,
                            &DispatcherQueueHandler::new(move || {
                                if let Some(tab_view) = weak.upgrade() {
                                    tab_view
                                        .SetSelectedItem(&selected)
                                        .expect("TabView::SetSelectedItem");
                                    tracing::info!(
                                        "TabView retry: index={:?} item_ok={}",
                                        tab_view.SelectedIndex(),
                                        tab_view.SelectedItem().is_ok(),
                                    );
                                }
                            }),
                        )
                        .expect("DispatcherQueue::TryEnqueueWithPriority");
                })
                .expect("Loaded"),
        );
    }

    let selection = layout.selection.clone();
    let ids_for_event = ids.clone();
    let revoker = tab_view
        .SelectionChanged(move |sender, _args| {
            let Ok(sender) = sender.ok() else {
                return;
            };
            let tab_view = sender.cast::<TabView>().expect("sender is the TabView");
            let index = tab_view.SelectedIndex().expect("TabView::SelectedIndex");
            tracing::info!(
                "TabView SelectionChanged: index={index} item_ok={}",
                tab_view.SelectedItem().is_ok()
            );
            if index >= 0
                && let Some(id) = ids_for_event
                    .as_slice()
                    .get(usize::try_from(index).expect("index non-negative"))
                    .copied()
                && selection.get() != id
            {
                selection.set(id);
            }
        })
        .expect("TabView::SelectionChanged");
    let element = framework(&tab_view.cast().expect("UIElement"));
    store_event_revoker(&element, revoker);
    store_event_revoker(&element, size_revoker);
    if let Some(loaded_revoker) = loaded_revoker {
        store_event_revoker(&element, loaded_revoker);
    }

    let queue = renderer.executor().queue().clone();
    let weak = tab_view.downgrade().expect("weak ref");
    guards.push(layout.selection.watch(move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        let ids = ids.clone();
        let items = items.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(tab_view) = weak.upgrade()
                && let Some(index) = ids.iter().position(|id| *id == value)
            {
                // `SelectedItem` is a no-op when already selected, so this
                // cannot feed back into the `SelectionChanged` handler.
                tab_view
                    .SetSelectedItem(&items[index])
                    .expect("SetSelectedItem");
            }
        });
    }));

    store_watcher_guards(&element, guards);

    tracing::info!("render_tab_view complete");
    let queue2 = renderer.executor().queue().clone();
    enqueue_on_ui_thread(&queue2, || {
        tracing::info!("dispatcher callback ran");
    });
    element.cast().expect("FrameworkElement is a UIElement")
}

#[allow(clippy::too_many_lines)] // flat registration/wiring code
fn render_sidebar_tabs(
    layout: TabsLayout,
    env: &Environment,
    renderer: &mut WinUiRenderer,
) -> UIElement {
    let nav = NavigationView::new().expect("NavigationView::new");
    nav.cast::<INavigationView2>()
        .expect("NavigationView supports INavigationView2")
        .SetPaneDisplayMode(NavigationViewPaneDisplayMode::Left)
        .expect("NavigationView::SetPaneDisplayMode");
    nav.SetIsSettingsVisible(false)
        .expect("NavigationView::SetIsSettingsVisible");

    let menu_items = crate::util::vector::<_, windows_core::IInspectable>(
        &nav.MenuItems().expect("NavigationView::MenuItems"),
    );

    let ids: Vec<Id> = layout.tabs.iter().map(|tab| tab.id).collect();
    let mut guards = Vec::new();

    let mut contents: Vec<UIElement> = Vec::with_capacity(layout.tabs.len());
    for tab in layout.tabs {
        contents.push(renderer.render_any(tab.content.build().content, env));
        let item = NavigationViewItem::new().expect("NavigationViewItem::new");
        let queue = renderer.executor().queue().clone();
        let header = tab_header(
            tab.label,
            tab.icon,
            tab.badge,
            env,
            renderer,
            &queue,
            &mut guards,
        );
        item.cast::<ContentControl>()
            .expect("NavigationViewItem is a ContentControl")
            .SetContent(&header)
            .expect("ContentControl::SetContent");
        item.cast::<Control>()
            .expect("NavigationViewItem is a Control")
            .SetIsEnabled(tab.enabled.get())
            .expect("Control::SetIsEnabled");
        menu_items
            .Append(
                &item
                    .cast::<windows_core::IInspectable>()
                    .expect("IInspectable"),
            )
            .expect("IVector::Append");
    }

    let contents = Rc::new(contents);
    let content_host = nav
        .cast::<ContentControl>()
        .expect("NavigationView is a ContentControl");
    let show = {
        let content_host = content_host;
        let contents = contents.clone();
        Rc::new(move |index: usize| {
            content_host
                .SetContent(&contents[index])
                .expect("NavigationView::SetContent");
        })
    };

    if let Some(index) = ids.iter().position(|id| *id == layout.selection.get()) {
        let selected = menu_items
            .GetAt(u32::try_from(index).expect("tab index fits u32"))
            .expect("IVector::GetAt");
        nav.SetSelectedItem(&selected).expect("SetSelectedItem");
        show(index);
    }

    let selection = layout.selection.clone();
    let ids_for_event = ids.clone();
    let show_for_event = show.clone();
    let revoker = nav
        .SelectionChanged(move |sender, _args| {
            let Ok(sender) = sender.ok() else {
                return;
            };
            let nav = sender
                .cast::<NavigationView>()
                .expect("sender is the NavigationView");
            let selected = nav.SelectedItem().expect("SelectedItem");
            let menu_items = crate::util::vector::<_, windows_core::IInspectable>(
                &nav.MenuItems().expect("MenuItems"),
            );
            let mut index = 0u32;
            if menu_items
                .IndexOf(&selected, &mut index)
                .expect("IVector::IndexOf")
            {
                let index = index as usize;
                show_for_event(index);
                if let Some(id) = ids_for_event.as_slice().get(index).copied()
                    && selection.get() != id
                {
                    selection.set(id);
                }
            }
        })
        .expect("NavigationView::SelectionChanged");
    let element = framework(&nav.cast().expect("UIElement"));
    store_event_revoker(&element, revoker);

    let queue = renderer.executor().queue().clone();
    let weak = nav.downgrade().expect("weak ref");
    guards.push(layout.selection.watch(move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        let ids = ids.clone();
        let show = show.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(nav) = weak.upgrade()
                && let Some(index) = ids.iter().position(|id| *id == value)
            {
                let item = crate::util::vector::<_, windows_core::IInspectable>(
                    &nav.MenuItems().expect("MenuItems"),
                )
                .GetAt(u32::try_from(index).expect("tab index fits u32"))
                .expect("GetAt");
                nav.SetSelectedItem(&item).expect("SetSelectedItem");
                show(index);
            }
        });
    }));

    store_watcher_guards(&element, guards);
    element.cast().expect("FrameworkElement is a UIElement")
}
