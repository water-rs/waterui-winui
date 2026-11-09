//! The machinery behind `cargo run -p winui-bindgen` and `waterui-winui`'s
//! `build.rs`: fetches the pinned Windows App SDK metadata (hash-verified),
//! compiles `extras.rdl` (COM interfaces absent from the SDK metadata), and
//! runs `windows-bindgen` over the filter lists.
//!
//! The `.winmd` inputs are not committed: [`download_nupkg`] pulls the
//! `Microsoft.WindowsAppSDK.Runtime` package at [`WASDK_VERSION`] from
//! nuget.org and [`extract_winmds`] lifts every `Microsoft.*.winmd` out of the
//! x64 MSIX it bundles. Both steps verify the package against
//! [`WASDK_RUNTIME_SHA256`], so a tampered or re-published payload fails the
//! build instead of silently changing the bindings.

use std::fmt::Write as _;
use std::io::{Cursor, Read};
use std::path::Path;

/// Pinned Windows App SDK runtime version the bindings target — the version
/// `windows-reactor` `2.4.0` (the WASDK release line this backend compiles
/// against) and what `cargo run -p winui-bindgen -- fetch` used to vendor
/// `winmd/` when it was committed.
pub const WASDK_VERSION: &str = "2.4.0";

/// SHA-256 of `Microsoft.WindowsAppSDK.Runtime.{WASDK_VERSION}.nupkg` as served
/// by nuget.org's flat container (versioned package content is immutable).
pub const WASDK_RUNTIME_SHA256: &str =
    "93f48b096416ab7b908ea30690bba47de8264cd2f22dd7b0ad97187a86d00120";

/// nuget.org flat-container URL for the pinned package.
pub const WASDK_RUNTIME_NUGET: &str =
    "https://www.nuget.org/api/v2/package/Microsoft.WindowsAppSDK.Runtime";

/// x64 runtime archive inside the nupkg that carries the metadata.
const MSIX_IN_NUPKG: &str = "tools/MSIX/win10-x64/Microsoft.WindowsAppRuntime.2.msix";

/// Interfaces generated with `*_Impl` traits so the backend can author them.
const IMPLEMENTS: &[&str] = &[
    "Microsoft.UI.Xaml.IApplicationOverrides",
    "Microsoft.UI.Xaml.Markup.IXamlMetadataProvider",
    "Microsoft.UI.Xaml.IElementFactory",
    "Microsoft.UI.Xaml.IFrameworkElementOverrides",
    "Windows.Graphics.IGeometrySource2D",
    "extras.IGeometrySource2DInterop",
];

/// Composable runtime classes the backend derives from.
const COMPOSES: &[&str] = &[
    "Microsoft.UI.Xaml.Application",
    "Microsoft.UI.Xaml.Controls.Panel",
];

type Error = Box<dyn std::error::Error>;

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// True when `bytes` is the pinned nupkg (SHA-256 == [`WASDK_RUNTIME_SHA256`]).
#[must_use]
pub fn verify_nupkg(bytes: &[u8]) -> bool {
    sha256_hex(bytes) == WASDK_RUNTIME_SHA256
}

/// Downloads the pinned WASDK runtime nupkg from nuget.org and verifies its
/// SHA-256 against [`WASDK_RUNTIME_SHA256`]. A mismatch is fatal: the bindings
/// must come from exactly the pinned metadata.
///
/// # Errors
///
/// Fails on any HTTP error from nuget.org and when the downloaded bytes do
/// not match [`WASDK_RUNTIME_SHA256`].
pub fn download_nupkg() -> Result<Vec<u8>, Error> {
    let url = format!("{WASDK_RUNTIME_NUGET}/{WASDK_VERSION}");
    // ureq buffers a response under a 10 MB default limit; the nupkg is ~164 MB.
    let bytes = ureq::get(&url)
        .call()?
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()?;
    if !verify_nupkg(&bytes) {
        return Err(format!(
            "Microsoft.WindowsAppSDK.Runtime {WASDK_VERSION} hash mismatch: \
             expected {WASDK_RUNTIME_SHA256}, got {} — refusing to bind \
             against unverified metadata",
            sha256_hex(&bytes)
        )
        .into());
    }
    Ok(bytes)
}

/// Opens the x64 MSIX inside the WASDK runtime nupkg (itself a zip) and
/// extracts every `Microsoft.*.winmd` into `dir`.
///
/// # Errors
///
/// Fails when the nupkg or the bundled MSIX is not a readable zip, when the
/// MSIX is missing, and on any filesystem error writing `dir`.
pub fn extract_winmds(nupkg: &[u8], dir: &Path) -> Result<(), Error> {
    let mut archive = zip::ZipArchive::new(Cursor::new(nupkg))?;
    let mut msix_bytes = Vec::new();
    archive
        .by_name(MSIX_IN_NUPKG)?
        .read_to_end(&mut msix_bytes)?;
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
    }
    Ok(())
}

/// Compiles `rdl` into `out` (`extras.winmd`) so bindgen sees the native COM
/// interfaces (`IWindowNative`, `ISwapChainPanelNative`, …).
///
/// # Errors
///
/// Fails when `rdl` cannot be read or compiled, and on any filesystem error
/// writing `out`.
pub fn compile_extras(rdl: &Path, out: &Path) -> Result<(), Error> {
    windows_rdl::Reader::new()
        .input(rdl)
        .reference_bytes(windows_default::WIN32)
        .output(out)
        .write()?;
    Ok(())
}

/// Runs `windows-bindgen` over `winmd_dir` with `filters`, writing `output`.
pub fn generate(winmd_dir: &Path, filters: &[impl AsRef<Path>], output: &Path) {
    let mut builder = windows_bindgen::builder();
    builder
        .input(winmd_dir)
        .input_default()
        .output(output)
        .implements(IMPLEMENTS.iter().copied());
    for name in COMPOSES {
        builder.compose(name);
    }
    builder
        .minimal()
        .dead_code()
        .flat()
        .filter_files(filters.iter().map(AsRef::as_ref))
        .write();
}
