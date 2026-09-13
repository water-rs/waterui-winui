//! Navigation: `NavigationView`, `NavigationStack`, `NavigationSplitLayout`.

use std::cell::RefCell;
use std::rc::Rc;

use nami::Signal;
use waterui_controls::text_field::TextField;
use waterui_core::id::Id;
use waterui_core::{AnyView, Environment};
use waterui_navigation::{
    Bar, CustomNavigationController, NavigationController, NavigationSplitColumnVisibility,
    NavigationSplitLayout, NavigationStack, NavigationToolbarPlacement, NavigationTransaction,
    NavigationView, navigation_back_label, resolve_navigation_root,
};
use windows_core::{IInspectable, Interface};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;
use crate::component::WinUiComponent;
use crate::executor::enqueue_on_ui_thread;
use crate::renderer::WinUiRenderer;
use crate::util::{
    children, framework, solid_brush, store_event_revoker, store_watcher_guards,
    subscribe_then_get, vector,
};

/// Grid rows inside one stack level: header bar, optional search row, the
/// destination content, and the bottom toolbar.
const HEADER_ROW: i32 = 0;
const SEARCH_ROW: i32 = 1;
const CONTENT_ROW: i32 = 2;
const BOTTOM_ROW: i32 = 3;

/// Visibility setter lives on `IUIElement`; cast anything into it.
fn set_visible(element: &impl Interface, hidden: bool) {
    element
        .cast::<UIElement>()
        .expect("element is a UIElement")
        .SetVisibility(if hidden {
            Visibility::Collapsed
        } else {
            Visibility::Visible
        })
        .expect("UIElement::SetVisibility");
}

/// Builds the chrome for one stack level around `content`: a `Grid` with a
/// three-column header row (back + leading | title | trailing), a collapsible
/// search row, the content, and a collapsible bottom toolbar row. All watchers
/// and event subscriptions install on elements inside the returned element,
/// so they die with it.
///
/// `back` is invoked when the level's back button is clicked; the button is
/// hidden when `back` is `None` (the root level).
#[allow(clippy::too_many_lines)] // flat registration/wiring code
fn build_level(
    bar: Bar,
    content: UIElement,
    env: &Environment,
    renderer: &mut WinUiRenderer,
    back: Option<WinUiNavigationController>,
) -> UIElement {
    let root = Grid::new().expect("Grid::new");
    let rows = vector::<_, RowDefinition>(&root.RowDefinitions().expect("RowDefinitions"));
    for unit in [
        GridUnitType::Auto,
        GridUnitType::Auto,
        GridUnitType::Star,
        GridUnitType::Auto,
    ] {
        let row = RowDefinition::new().expect("RowDefinition::new");
        row.SetHeight(GridLength {
            value: 1.0,
            grid_unit_type: unit,
        })
        .expect("RowDefinition::SetHeight");
        rows.Append(&row).expect("Append row");
    }

    let header = Grid::new().expect("Grid::new");
    let header_columns =
        vector::<_, ColumnDefinition>(&header.ColumnDefinitions().expect("ColumnDefinitions"));
    for unit in [GridUnitType::Auto, GridUnitType::Star, GridUnitType::Auto] {
        let column = ColumnDefinition::new().expect("ColumnDefinition::new");
        column
            .SetWidth(GridLength {
                value: 1.0,
                grid_unit_type: unit,
            })
            .expect("ColumnDefinition::SetWidth");
        header_columns.Append(&column).expect("Append column");
    }

    let leading_cell = StackPanel::new().expect("StackPanel::new");
    leading_cell
        .SetOrientation(Orientation::Horizontal)
        .expect("SetOrientation");
    leading_cell.SetSpacing(8.0).expect("SetSpacing");
    let leading_children = children(&leading_cell);

    if let Some(back) = back {
        let back_button = Button::new().expect("Button::new");
        let back_content = StackPanel::new().expect("StackPanel::new");
        back_content
            .SetOrientation(Orientation::Horizontal)
            .expect("SetOrientation");
        back_content.SetSpacing(4.0).expect("SetSpacing");
        let back_icon = SymbolIcon::new().expect("SymbolIcon::new");
        back_icon
            .SetSymbol(Symbol::Back)
            .expect("SymbolIcon::SetSymbol");
        let back_content_children = children(&back_content);
        back_content_children
            .Append(&back_icon.cast::<UIElement>().expect("UIElement"))
            .expect("Append icon");
        // The localized "Back" label keeps its accessibility text.
        back_content_children
            .Append(&renderer.render_any(AnyView::new(navigation_back_label()), env))
            .expect("Append label");
        back_button
            .cast::<ContentControl>()
            .expect("Button is a ContentControl")
            .SetContent(&back_content.cast::<IInspectable>().expect("IInspectable"))
            .expect("ContentControl::SetContent");
        let revoker = back_button
            .cast::<ButtonBase>()
            .expect("Button is a ButtonBase")
            .Click(move |_sender, _args| {
                back.request_back();
            })
            .expect("ButtonBase::Click");
        let back_element = back_button.cast::<UIElement>().expect("UIElement");
        store_event_revoker(&framework(&back_element), revoker);
        leading_children
            .Append(&back_element)
            .expect("Append back button");
    }

    let title_box = StackPanel::new().expect("StackPanel::new");
    title_box
        .SetOrientation(Orientation::Vertical)
        .expect("SetOrientation");
    let title_children = children(&title_box);
    title_children
        .Append(&renderer.render_any(bar.title, env))
        .expect("Append title");
    if !bar.subtitle.is::<()>() {
        title_children
            .Append(&renderer.render_any(bar.subtitle, env))
            .expect("Append subtitle");
    }

    let trailing = StackPanel::new().expect("StackPanel::new");
    trailing
        .SetOrientation(Orientation::Horizontal)
        .expect("SetOrientation");
    trailing.SetSpacing(8.0).expect("SetSpacing");
    let trailing_children = children(&trailing);

    let bottom = StackPanel::new().expect("StackPanel::new");
    bottom
        .SetOrientation(Orientation::Horizontal)
        .expect("SetOrientation");
    bottom.SetSpacing(8.0).expect("SetSpacing");
    let bottom_children = children(&bottom);

    for item in bar.toolbar.items {
        let element = renderer.render_any(item.content, env);
        let target = match item.placement {
            NavigationToolbarPlacement::Cancellation
            | NavigationToolbarPlacement::TopBarLeading => &leading_children,
            NavigationToolbarPlacement::BottomBar | NavigationToolbarPlacement::Status => {
                &bottom_children
            }
            NavigationToolbarPlacement::Principal
            | NavigationToolbarPlacement::PrimaryAction
            | NavigationToolbarPlacement::SecondaryAction
            | NavigationToolbarPlacement::Confirmation
            | NavigationToolbarPlacement::TopBarTrailing => &trailing_children,
        };
        target.Append(&element).expect("Append toolbar item");
    }

    let leading_ui = leading_cell.cast::<UIElement>().expect("UIElement");
    let title_ui = title_box.cast::<UIElement>().expect("UIElement");
    let trailing_ui = trailing.cast::<UIElement>().expect("UIElement");
    Grid::SetColumn(&framework(&leading_ui), 0).expect("Grid::SetColumn");
    Grid::SetColumn(&framework(&title_ui), 1).expect("Grid::SetColumn");
    Grid::SetColumn(&framework(&trailing_ui), 2).expect("Grid::SetColumn");
    let header_children = children(&header);
    header_children.Append(&leading_ui).expect("Append leading");
    header_children.Append(&title_ui).expect("Append title");
    header_children
        .Append(&trailing_ui)
        .expect("Append trailing");

    let search = ContentControl::new().expect("ContentControl::new");
    if let Some(search_config) = &bar.search {
        let search_field = TextField::new(search_config.prompt.clone(), &search_config.text);
        search
            .SetContent(
                &renderer
                    .render(search_field, env)
                    .cast::<IInspectable>()
                    .expect("IInspectable"),
            )
            .expect("SetContent");
    } else {
        set_visible(&search, true);
    }

    if bottom_children.Size().expect("Size") == 0 {
        set_visible(&bottom, true);
    }

    if let Some(color) = &bar.color {
        let resolved = color.expect_resolved();
        let header_for_watch = header.clone();
        let queue = renderer.executor().queue().clone();
        let (initial, guard) = subscribe_then_get(resolved, move |ctx| {
            let color = ctx.into_value();
            let header = header_for_watch.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                header
                    .cast::<Panel>()
                    .expect("Grid is a Panel")
                    .SetBackground(&solid_brush(&color).expect("SolidColorBrush"))
                    .expect("Panel::SetBackground");
            });
        });
        header
            .cast::<Panel>()
            .expect("Grid is a Panel")
            .SetBackground(&solid_brush(&initial).expect("SolidColorBrush"))
            .expect("Panel::SetBackground");
        store_watcher_guards(&framework(&header.cast().expect("UIElement")), vec![guard]);
    }

    let header_for_watch = header.clone();
    let queue = renderer.executor().queue().clone();
    let (hidden, guard) = subscribe_then_get(&bar.hidden, move |ctx| {
        let hidden = ctx.into_value();
        let header = header_for_watch.clone();
        let queue = queue.clone();
        enqueue_on_ui_thread(&queue, move || {
            set_visible(&header, hidden);
        });
    });
    set_visible(&header, hidden);
    store_watcher_guards(&framework(&header.cast().expect("UIElement")), vec![guard]);

    let root_children = children(&root);
    for (row, element) in [
        (HEADER_ROW, header.cast::<UIElement>().expect("UIElement")),
        (SEARCH_ROW, search.cast::<UIElement>().expect("UIElement")),
        (CONTENT_ROW, content),
        (BOTTOM_ROW, bottom.cast::<UIElement>().expect("UIElement")),
    ] {
        Grid::SetRow(&framework(&element), row).expect("Grid::SetRow");
        root_children.Append(&element).expect("Append row element");
    }

    root.cast().expect("Grid is a UIElement")
}

/// One destination pushed onto the `WinUI` stack: the level element carries
/// its own chrome and destination state.
struct StackEntry {
    state: waterui_navigation::NavigationDestinationState,
}

/// The `WinUI` navigation controller: applies `NavigationTransaction`s to a
/// `Grid` whose children are the level elements; only the top is visible.
///
/// The state is `Rc`-shared so the boxed trait object held by
/// `NavigationController` and the copies handed to back buttons see the same
/// stack.
#[derive(Clone)]
struct WinUiNavigationController {
    inner: Rc<RefCell<ControllerInner>>,
    env: Rc<RefCell<Environment>>,
}

struct ControllerInner {
    host: Grid,
    entries: Vec<StackEntry>,
    controller: Option<NavigationController>,
}

impl WinUiNavigationController {
    fn new(host: Grid, env: Environment) -> Self {
        Self {
            inner: Rc::new(RefCell::new(ControllerInner {
                host,
                entries: Vec::new(),
                controller: None,
            })),
            env: Rc::new(RefCell::new(env)),
        }
    }

    /// The back affordance asks the current destination whether the pop may
    /// start, then delegates to the controller, which reports back through a
    /// transaction.
    fn request_back(&self) {
        let env = self.env.borrow().clone();
        let allowed = self
            .inner
            .borrow_mut()
            .entries
            .last_mut()
            .is_none_or(|entry| entry.state.attempt_pop(&env));
        if allowed && let Some(controller) = &self.inner.borrow().controller {
            controller.request_pop(1);
        }
    }
}

impl CustomNavigationController for WinUiNavigationController {
    /// Applies one suffix-replacing transaction: pop `removed` entries above
    /// `retained_prefix`, then push `inserted`.
    fn apply(&mut self, transaction: NavigationTransaction) {
        let mut inner = self.inner.borrow_mut();
        let env = self.env.borrow().clone();
        let children = children(&inner.host);

        for _ in 0..transaction.removed {
            let entry = inner.entries.pop().expect("entry exists for removal");
            if let Some(mut disappear) = entry.state.disappear {
                disappear(&env);
            }
            if let Some(mut pop) = entry.state.pop {
                pop(&env);
            }
            children.RemoveAtEnd().expect("RemoveAtEnd");
        }

        for builder in transaction.inserted {
            let view = builder.build();
            let mut renderer =
                WinUiRenderer::for_current_thread().expect("rendering requires the UI thread");
            let content = renderer.render_any(view.content, &env);
            let level = build_level(view.bar, content, &env, &mut renderer, Some(self.clone()));
            children.Append(&level).expect("Append destination");
            inner.entries.push(StackEntry { state: view.state });
        }

        // Only the top destination is visible.
        let count = children.Size().expect("Children::Size");
        for index in 0..count {
            let element = children
                .GetAt(index)
                .expect("GetAt")
                .cast::<UIElement>()
                .expect("level is a UIElement");
            set_visible(&element, index + 1 != count);
        }

        if let Some(top) = inner.entries.last_mut()
            && let Some(appear) = &mut top.state.appear
        {
            appear(&env);
        }

        if let Some(controller) = &inner.controller {
            let _ = controller.transition_completed(transaction.id);
        }
    }
}

impl WinUiComponent for NavigationView {
    /// Without a `NavigationController` in the environment this renders a
    /// standalone bar + content layout; inside a stack the stack owns the
    /// chrome and only the content renders.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let content = renderer.render_any(self.content, env);
        if env.get::<NavigationController>().is_some() {
            return content;
        }
        build_level(self.bar, content, env, renderer, None)
    }
}

impl WinUiComponent for NavigationStack<(), ()> {
    /// Creates the stack host, injects a `NavigationController` into the
    /// child environment, and projects the resolved root.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let root = self.into_inner();

        let host = Grid::new().expect("Grid::new");
        let controller_impl = WinUiNavigationController::new(host.clone(), env.clone());
        let navigation_controller = NavigationController::new(controller_impl.clone());
        controller_impl.inner.borrow_mut().controller = Some(navigation_controller.clone());
        let mut child_env = env.clone();
        child_env.insert(navigation_controller);
        *controller_impl.env.borrow_mut() = child_env.clone();

        let NavigationView { bar, content, .. } = resolve_navigation_root(root, &child_env);
        let content = renderer.render_any(content, &child_env);
        let level = build_level(bar, content, &child_env, renderer, None);
        children(&host).Append(&level).expect("Append root");

        host.cast().expect("Grid is a UIElement")
    }
}

impl WinUiComponent for NavigationSplitLayout {
    /// Renders a `WinUI` `NavigationView` with the sidebar as pane content and
    /// the detail column swapped by the primary selection.
    fn render(self, env: &Environment, renderer: &mut WinUiRenderer) -> UIElement {
        let (
            sidebar,
            placeholder,
            primary_selection,
            _content_builder,
            _secondary_selection,
            detail_builder,
            column_visibility,
            sidebar_width,
            _style,
        ) = self.into_parts();

        let nav = crate::bindings::NavigationView::new().expect("NavigationView::new");
        let nav2 = nav
            .cast::<INavigationView2>()
            .expect("NavigationView supports INavigationView2");
        nav2.SetPaneDisplayMode(NavigationViewPaneDisplayMode::Left)
            .expect("INavigationView2::SetPaneDisplayMode");
        nav.SetIsSettingsVisible(false)
            .expect("NavigationView::SetIsSettingsVisible");
        nav.SetOpenPaneLength(f64::from(sidebar_width.ideal()))
            .expect("SetOpenPaneLength");
        nav.SetIsPaneOpen(matches!(
            column_visibility.get(),
            NavigationSplitColumnVisibility::Automatic
                | NavigationSplitColumnVisibility::All
                | NavigationSplitColumnVisibility::DoubleColumn
        ))
        .expect("SetIsPaneOpen");

        let detail_for = {
            let env = env.clone();
            Rc::new(move |selection: Option<Id>| -> UIElement {
                let mut detail_renderer = WinUiRenderer::for_current_thread()
                    .expect("detail rendering requires the UI thread");
                match selection {
                    Some(id) => {
                        detail_renderer.render_any(AnyView::new(detail_builder.build(id)), &env)
                    }
                    None => detail_renderer.render_any(placeholder.build(), &env),
                }
            })
        };

        nav.cast::<ContentControl>()
            .expect("ContentControl")
            .SetContent(
                &detail_for(primary_selection.get())
                    .cast::<IInspectable>()
                    .expect("IInspectable"),
            )
            .expect("SetContent");

        nav2.SetPaneCustomContent(&renderer.render_any(sidebar.build(), env))
            .expect("INavigationView2::SetPaneCustomContent");

        let mut guards = Vec::new();
        let queue = renderer.executor().queue().clone();
        let weak = nav.downgrade().expect("weak ref");
        guards.push(primary_selection.watch(move |ctx| {
            let selection = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            let detail_for = detail_for.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(nav) = weak.upgrade() {
                    nav.cast::<ContentControl>()
                        .expect("ContentControl")
                        .SetContent(
                            &detail_for(selection)
                                .cast::<IInspectable>()
                                .expect("IInspectable"),
                        )
                        .expect("SetContent");
                }
            });
        }));

        let queue = renderer.executor().queue().clone();
        let weak = nav.downgrade().expect("weak ref");
        guards.push(column_visibility.watch(move |ctx| {
            let visibility = ctx.into_value();
            let weak = weak.clone();
            let queue = queue.clone();
            enqueue_on_ui_thread(&queue, move || {
                if let Some(nav) = weak.upgrade() {
                    nav.SetIsPaneOpen(matches!(
                        visibility,
                        NavigationSplitColumnVisibility::Automatic
                            | NavigationSplitColumnVisibility::All
                            | NavigationSplitColumnVisibility::DoubleColumn
                    ))
                    .expect("SetIsPaneOpen");
                }
            });
        }));

        let element = framework(&nav.cast().expect("UIElement"));
        store_watcher_guards(&element, guards);
        element.cast().expect("FrameworkElement is a UIElement")
    }
}
