//! Regenerates `src/bindings.rs` for `waterui-winui` from the Windows App SDK
//! `.winmd` files vendored under `winmd/`, plus `extras.winmd` compiled from
//! `tools/bindgen/extras.rdl` (COM interfaces absent from the SDK metadata).
//!
//! Usage:
//!   cargo run -p winui-bindgen           regenerate bindings from `winmd/`
//!   cargo run -p winui-bindgen -- fetch  download the pinned WASDK runtime
//!                                        package and refresh `winmd/`

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const WINMD_DIR: &str = "winmd";
const EXTRAS_RDL: &str = "tools/bindgen/extras.rdl";
const EXTRAS_WINMD: &str = "winmd/extras.winmd";
const RUNTIME_FILTER: &str = "tools/bindgen/runtime.txt";
const CONTROLS_FILTER: &str = "tools/bindgen/controls.txt";
const OUTPUT_FILE: &str = "src/bindings.rs";

/// Pinned Windows App SDK runtime version the bindings target.
const WASDK_VERSION: &str = "2.4.0";
const WASDK_RUNTIME_NUGET: &str =
    "https://www.nuget.org/api/v2/package/Microsoft.WindowsAppSDK.Runtime";
const MSIX_IN_NUPKG: &str = "tools/MSIX/win10-x64/Microsoft.WindowsAppRuntime.2.msix";

/// Interfaces generated with `*_Impl` traits so the backend can author them.
const IMPLEMENTS: &[&str] = &[
    "Microsoft.UI.Xaml.IApplicationOverrides",
    "Microsoft.UI.Xaml.Markup.IXamlMetadataProvider",
    "Microsoft.UI.Xaml.IElementFactory",
    "Microsoft.UI.Xaml.IFrameworkElementOverrides",
];

/// Composable runtime classes the backend derives from.
const COMPOSES: &[&str] = &[
    "Microsoft.UI.Xaml.Application",
    "Microsoft.UI.Xaml.Controls.Panel",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tools/bindgen must live two levels below the workspace root")
        .to_path_buf()
}

fn main() -> ExitCode {
    let root = workspace_root();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("query") {
        return query(&root, &args[1..]);
    }
    if args.iter().any(|arg| arg == "fetch")
        && let Err(error) = fetch_winmds(&root.join(WINMD_DIR))
    {
        eprintln!("fetch failed: {error}");
        return ExitCode::FAILURE;
    }

    if let Err(error) = generate_extras(&root) {
        eprintln!("extras winmd failed: {error}");
        return ExitCode::FAILURE;
    }
    generate(&root);
    ExitCode::SUCCESS
}

/// Downloads the pinned WASDK runtime `NuGet` package, opens the bundled x64 MSIX
/// (itself a zip), and extracts every `Microsoft.*.winmd` into `dir`.
fn fetch_winmds(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{WASDK_RUNTIME_NUGET}/{WASDK_VERSION}");
    let bytes = ureq::get(&url).call()?.body_mut().read_to_vec()?;
    let mut nupkg = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut msix_bytes = Vec::new();
    nupkg.by_name(MSIX_IN_NUPKG)?.read_to_end(&mut msix_bytes)?;
    let mut msix = zip::ZipArchive::new(Cursor::new(msix_bytes))?;

    std::fs::create_dir_all(dir)?;
    for index in 0..msix.len() {
        let mut entry = msix.by_index(index)?;
        let name = entry.name().to_owned();
        if !(name.starts_with("Microsoft.") && name.to_lowercase().ends_with(".winmd")) {
            continue;
        }
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents)?;
        std::fs::write(dir.join(&name), &contents)?;
        println!("wrote {name}");
    }
    Ok(())
}

/// Compiles `extras.rdl` into `winmd/extras.winmd` so bindgen sees the native
/// COM interfaces (`IWindowNative`, `ISwapChainPanelNative`, …).
fn generate_extras(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    windows_rdl::Reader::new()
        .input(root.join(EXTRAS_RDL))
        .reference_bytes(windows_default::WIN32)
        .output(root.join(EXTRAS_WINMD))
        .write()?;
    Ok(())
}

/// Prints the namespace and method names of every type matching `names`.
///
/// Usage: `cargo run -p winui-bindgen -- query HoldingState PointerPoint`
fn query(root: &Path, names: &[String]) -> ExitCode {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join(WINMD_DIR)).expect("winmd dir") {
        let path = entry.expect("winmd entry").path();
        if path.extension().is_some_and(|ext| ext == "winmd")
            && let Some(file) = windows_metadata::reader::File::read(&path)
        {
            files.push(file);
        }
    }
    files.push(
        windows_metadata::reader::File::new(windows_default::WINRT.to_vec())
            .expect("windows-default winmd"),
    );
    let index = windows_metadata::reader::Index::new(files);
    for name in names {
        let mut found = false;
        for (namespace, type_name, def) in index.iter() {
            if type_name != name.as_str() {
                continue;
            }
            found = true;
            println!("{namespace}.{name}");
            for method in def.methods() {
                println!("  .{}", method.name());
            }
        }
        if !found {
            println!("{name}: not found");
        }
    }
    ExitCode::SUCCESS
}

fn generate(root: &Path) {
    let output = root.join(OUTPUT_FILE);
    let mut builder = windows_bindgen::builder();
    builder
        .input(root.join(WINMD_DIR))
        .input_default()
        .output(&output)
        .implements(IMPLEMENTS.iter().copied());
    for name in COMPOSES {
        builder.compose(name);
    }
    builder
        .minimal()
        .dead_code()
        .flat()
        .filter_files([root.join(RUNTIME_FILTER), root.join(CONTROLS_FILTER)])
        .write();
    println!("wrote {}", output.display());
}
