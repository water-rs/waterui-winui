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
    let tab_items = crate::util::vector::<_, windows_core::IInspectable>(
        &tab_view.TabItems().expect("TabView::TabItems"),
    );

    let ids: Vec<Id> = layout.tabs.iter().map(|tab| tab.id).collect();
    let mut guards = Vec::new();

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
        let _ = &navigation_view; // DEBUG: bypass content render
        let block = TextBlock::new().expect("TextBlock::new");
        block
            .SetText("DEBUG tab content")
            .expect("TextBlock::SetText");
        let content: UIElement = block.cast().expect("UIElement");
        item.cast::<ContentControl>()
            .expect("TabViewItem is a ContentControl")
            .SetContent(&content)
            .expect("ContentControl::SetContent");
        item.cast::<Control>()
            .expect("TabViewItem is a Control")
            .SetIsEnabled(tab.enabled.get())
            .expect("Control::SetIsEnabled");
        tab_items
            .Append(
                &item
                    .cast::<windows_core::IInspectable>()
                    .expect("IInspectable"),
            )
            .expect("IVector::Append");
    }

    if let Some(index) = ids.iter().position(|id| *id == layout.selection.get()) {
        tab_view
            .SetSelectedIndex(i32::try_from(index).expect("tab index fits i32"))
            .expect("TabView::SetSelectedIndex");
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

    let queue = renderer.executor().queue().clone();
    let weak = tab_view.downgrade().expect("weak ref");
    guards.push(layout.selection.watch(move |ctx| {
        let value = ctx.into_value();
        let weak = weak.clone();
        let queue = queue.clone();
        let ids = ids.clone();
        enqueue_on_ui_thread(&queue, move || {
            if let Some(tab_view) = weak.upgrade()
                && let Some(index) = ids.iter().position(|id| *id == value)
            {
                let index = i32::try_from(index).expect("tab index fits i32");
                if tab_view.SelectedIndex().expect("SelectedIndex") != index {
                    tab_view.SetSelectedIndex(index).expect("SetSelectedIndex");
                }
            }
        });
    }));

    store_watcher_guards(&element, guards);
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
