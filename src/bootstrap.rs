//! Windows App Runtime bootstrap for unpackaged processes.
//!
//! `WinUI` 3 lives in the `Microsoft.WindowsAppRuntime` framework package, not in
//! the OS. A packaged (MSIX) process already has it in its package graph; an
//! unpackaged process must resolve and add it dynamically before touching any
//! `Microsoft.UI` type.

use std::path::Path;

use windows_core::{HRESULT, PCWSTR, PWSTR, w};

#[allow(clippy::wildcard_imports)] // the generated namespace
use crate::bindings::*;

const FRAMEWORK_FAMILY: PCWSTR = w!("Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe");
const PACKAGE_DEPENDENCY_LIFETIME_KIND_PROCESS: i32 = 0;
const SELF_CONTAINED_MARKER: &str = "waterui-winui-self-contained";

windows_core::link!("kernel32.dll" "system" fn FindResourceW(module: *mut core::ffi::c_void, name: *const u16, resource_type: *const u16) -> *mut core::ffi::c_void);
windows_core::link!("kernel32.dll" "system" fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void);
windows_core::link!("kernel32.dll" "system" fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut core::ffi::c_void) -> i32);
windows_core::link!("kernel32.dll" "system" fn LoadResource(module: *mut core::ffi::c_void, resource: *mut core::ffi::c_void) -> *mut core::ffi::c_void);
windows_core::link!("kernel32.dll" "system" fn LockResource(resource: *mut core::ffi::c_void) -> *mut core::ffi::c_void);
windows_core::link!("kernel32.dll" "system" fn SizeofResource(module: *mut core::ffi::c_void, resource: *mut core::ffi::c_void) -> u32);

/// Whether the current process has MSIX package identity.
pub fn is_packaged_process() -> windows_core::Result<bool> {
    let mut length = 0;
    let rc = unsafe { GetCurrentPackageFullName(&raw mut length, PWSTR::null()) };
    match rc {
        ERROR_INSUFFICIENT_BUFFER => Ok(true),
        APPMODEL_ERROR_NO_PACKAGE => Ok(false),
        other => Err(HRESULT(other | 0x8007_0000u32.cast_signed()).into()),
    }
}

/// Ensures the Windows App Runtime is usable in this process.
///
/// Packaged processes and self-contained apps are detected and skipped;
/// unpackaged framework-dependent apps resolve the installed runtime package
/// into the process package graph. If the framework is missing, an error is
/// returned after offering the user the runtime download page.
///
/// Called once by [`crate::run_app`] before `Application::Start`; calling it
/// again would create a second package dependency.
pub fn bootstrap_runtime() -> windows_core::Result<()> {
    ensure_runtime()
}

fn ensure_runtime() -> windows_core::Result<()> {
    if is_packaged_process()? {
        return Ok(());
    }
    if self_contained_manifest_present() {
        return if self_contained_runtime_present() {
            Ok(())
        } else {
            Err(windows_core::Error::new(
                // HRESULT_FROM_WIN32(ERROR_MOD_NOT_FOUND)
                HRESULT(0x8007_007Eu32.cast_signed()),
                "self-contained Windows App Runtime files are missing",
            ))
        };
    }
    bootstrap_framework_dependency()
}

/// Adds the Windows App Runtime framework package to this process's package
/// graph for the lifetime of the process.
fn bootstrap_framework_dependency() -> windows_core::Result<()> {
    let mut dependency_id = PWSTR::null();
    let create = unsafe {
        TryCreatePackageDependency(
            std::ptr::null_mut(),
            FRAMEWORK_FAMILY,
            PACKAGE_VERSION {
                Anonymous: PACKAGE_VERSION_0 {
                    Version: WINDOWSAPPSDK_RUNTIME_VERSION_UINT64,
                },
            },
            process_architecture_flags() | PackageDependencyProcessorArchitectures_Neutral,
            PACKAGE_DEPENDENCY_LIFETIME_KIND_PROCESS,
            PCWSTR::null(),
            0,
            &raw mut dependency_id,
        )
    };

    if create == STATEREPOSITORY_E_DEPENDENCY_NOT_RESOLVED {
        show_install_dialog();
        return Err(windows_core::Error::new(
            create,
            "Microsoft.WindowsAppRuntime framework package is not installed",
        ));
    }
    create.ok()?;

    // The context handle is intentionally unused: the dependency is scoped to
    // the process lifetime, so nothing ever removes it.
    let mut context = std::ptr::null_mut();
    let mut package_full_name = PWSTR::null();
    let add = unsafe {
        AddPackageDependency(
            PCWSTR(dependency_id.0),
            0,
            0,
            &raw mut context,
            &raw mut package_full_name,
        )
    };
    unsafe {
        _ = HeapFree(GetProcessHeap(), 0, dependency_id.0.cast());
        _ = HeapFree(GetProcessHeap(), 0, package_full_name.0.cast());
    }

    if add == STATEREPOSITORY_E_DEPENDENCY_NOT_RESOLVED {
        show_install_dialog();
        return Err(windows_core::Error::new(
            add,
            "Microsoft.WindowsAppRuntime framework package is not installed",
        ));
    }
    add.ok()
}

fn process_architecture_flags() -> PackageDependencyProcessorArchitectures {
    match std::env::consts::ARCH {
        "x86_64" => PackageDependencyProcessorArchitectures_X64,
        "x86" => PackageDependencyProcessorArchitectures_X86,
        "aarch64" => PackageDependencyProcessorArchitectures_Arm64,
        "arm" => PackageDependencyProcessorArchitectures_Arm,
        _ => PackageDependencyProcessorArchitectures_None,
    }
}

fn process_caption() -> windows_core::HSTRING {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .map_or_else(
            || windows_core::HSTRING::from("This application could not be started"),
            windows_core::HSTRING::from,
        )
}

fn show_install_dialog() {
    let caption = process_caption();
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "x86",
        "aarch64" => "arm64",
        other => other,
    };
    let text = windows_core::HSTRING::from(format!(
        "You must install Windows App Runtime \
         ({WINDOWSAPPSDK_RUNTIME_VERSION_MAJOR}.{WINDOWSAPPSDK_RUNTIME_VERSION_MINOR}.\
         {WINDOWSAPPSDK_RUNTIME_VERSION_BUILD}.{WINDOWSAPPSDK_RUNTIME_VERSION_REVISION}, \
         {arch}) to run this application.\n\nDo you want to download it now?"
    ));

    let result = unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR::from_raw(text.as_ptr()),
            PCWSTR::from_raw(caption.as_ptr()),
            (MB_YESNO | MB_ICONERROR) as u32,
        )
    };
    if result == IDYES {
        unsafe {
            ShellExecuteW(
                HWND::default(),
                w!("open"),
                w!("https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads"),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

fn self_contained_runtime_present() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .is_some_and(|dir| dir.join("Microsoft.WindowsAppRuntime.dll").is_file())
}

/// Looks for the marker string embedded into the application manifest when a
/// build script stages a self-contained Windows App Runtime. The
/// `windows-reactor-setup` marker is recognized as well so that build script
/// can be reused unchanged.
fn self_contained_manifest_present() -> bool {
    unsafe {
        // The marker normally lives in the exe's embedded manifest. A CEF
        // bootstrap application is a DLL loaded by the renamed
        // `bootstrap.exe`/`bootstrapc.exe` launcher — the exe's manifest is
        // fixed at CEF build time, so the marker is embedded into the
        // application DLL instead. Probing this code's own module covers both:
        // for an exe build it resolves to the exe module, for the CEF DLL it
        // resolves to the DLL.
        const FROM_ADDRESS_UNCHANGED_REFCOUNT: u32 = 0x2 | 0x4;
        let exe = GetModuleHandleW(std::ptr::null());
        let mut this = std::ptr::null_mut();
        _ = GetModuleHandleExW(
            FROM_ADDRESS_UNCHANGED_REFCOUNT,
            (module_manifest_has_marker as fn(*mut core::ffi::c_void) -> bool) as *const u16,
            &raw mut this,
        );
        module_manifest_has_marker(exe) || (this != exe && module_manifest_has_marker(this))
    }
}

#[allow(clippy::manual_dangling_ptr)] // FindResourceW uses low pointer values for ordinals.
fn module_manifest_has_marker(module: *mut core::ffi::c_void) -> bool {
    const MARKERS: &[&str] = &[
        SELF_CONTAINED_MARKER,
        // windows-reactor-setup embeds this marker when it stages the runtime.
        "windows-reactor-self-contained",
    ];

    unsafe {
        if module.is_null() {
            return false;
        }
        let resource = FindResourceW(module, 1usize as *const u16, 24usize as *const u16);
        if resource.is_null() {
            return false;
        }
        let size = SizeofResource(module, resource) as usize;
        let loaded = LoadResource(module, resource);
        if loaded.is_null() {
            return false;
        }
        let data = LockResource(loaded).cast::<u8>();
        if data.is_null() {
            return false;
        }
        let manifest = std::slice::from_raw_parts(data, size);
        MARKERS.iter().any(|marker| {
            manifest
                .windows(marker.len())
                .any(|w| w == marker.as_bytes())
        })
    }
}

/// Initializes the UI thread: per-monitor DPI awareness then COM as an STA.
/// `WinUI` requires STA; `RPC_E_CHANGED_MODE` is a hard error.
pub fn initialize_ui_thread() -> windows_core::Result<()> {
    unsafe {
        _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let result = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    if result == RPC_E_CHANGED_MODE {
        return Err(windows_core::Error::new(
            RPC_E_CHANGED_MODE,
            "WinUI requires an STA thread",
        ));
    }
    result.ok()
}
