//! The composed `Application` shim.
//!
//! `WinUI`'s `Application` is a composable runtime class: the framework creates
//! the base object and calls back into our overrides. `WaterUI` provides
//! `IApplicationOverrides` (the real entry point, `OnLaunched`) and delegates
//! `IXamlMetadataProvider` to the XAML controls provider so XAML reflection
//! keeps working.

#![allow(clippy::inline_always, clippy::ref_as_ptr)] // generated `implement` macro items
use std::cell::RefCell;

use windows_core::{Array, Interface, Ref, implement};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

type OnLaunched = Box<dyn FnOnce() -> windows_core::Result<()>>;

#[implement(IApplicationOverrides, IXamlMetadataProvider)]
pub struct AppShim {
    controls_provider: RefCell<Option<XamlControlsXamlMetaDataProvider>>,
    on_launched: RefCell<Option<OnLaunched>>,
}

impl AppShim {
    pub fn new(on_launched: OnLaunched) -> Self {
        Self {
            controls_provider: RefCell::new(None),
            on_launched: RefCell::new(Some(on_launched)),
        }
    }

    fn provider(&self) -> windows_core::Result<XamlControlsXamlMetaDataProvider> {
        if let Some(provider) = self.controls_provider.borrow().as_ref() {
            return Ok(provider.clone());
        }
        let provider = XamlControlsXamlMetaDataProvider::new()?;
        *self.controls_provider.borrow_mut() = Some(provider.clone());
        Ok(provider)
    }
}

impl IApplicationOverrides_Impl for AppShim_Impl {
    fn OnLaunched(&self, _args: Ref<LaunchActivatedEventArgs>) -> windows_core::Result<()> {
        if let Some(on_launched) = self.on_launched.borrow_mut().take() {
            on_launched()?;
        }
        Ok(())
    }
}

impl IXamlMetadataProvider_Impl for AppShim_Impl {
    fn GetXamlType(&self, r#type: &TypeName) -> windows_core::Result<IXamlType> {
        self.provider()?.GetXamlType(r#type)
    }

    fn GetXamlTypeByFullName(
        &self,
        fullname: &windows_core::HSTRING,
    ) -> windows_core::Result<IXamlType> {
        self.provider()?
            .GetXamlTypeByFullName(&fullname.to_string_lossy())
    }

    fn GetXmlnsDefinitions(&self) -> windows_core::Result<Array<XmlnsDefinition>> {
        self.provider()?.GetXmlnsDefinitions()
    }
}

/// Creates the composed `Application` whose `OnLaunched` runs `on_launched`.
pub fn create_application(on_launched: OnLaunched) -> windows_core::Result<Application> {
    Application::compose(AppShim::new(on_launched))
}

/// Installs `XamlControlsResources` into the application's merged dictionaries.
/// Without this, controls render without Fluent styles.
pub fn install_xaml_controls_resources(application: &Application) -> windows_core::Result<()> {
    let controls = XamlControlsResources::new()?;
    crate::util::vector::<_, ResourceDictionary>(&application.Resources()?.MergedDictionaries()?)
        .Append(&controls.cast::<ResourceDictionary>()?)
}
