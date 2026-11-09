//! Build script for `waterui-winui`.
//!
//! On Windows targets it generates `src/bindings.rs` into `OUT_DIR`; the
//! crate `include!`s it, so generated output — and the Windows App SDK
//! `.winmd` metadata it reads — never lives in the repository (#93).
//!
//! With the `self-contained` feature, stages the Windows App Runtime next to
//! the produced binaries via `windows-reactor-setup` and embeds the marker
//! manifest that `bootstrap` uses to recognize the staged runtime. The package
//! has a bin target (`src/bin/smoke.rs`) so the `rustc-link-arg-bins`
//! instructions emitted by `as_self_contained` are valid.

use std::path::PathBuf;

#[path = "build/bindgen.rs"]
mod bindgen;

fn main() {
    #[cfg(feature = "self-contained")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_reactor_setup::as_self_contained();
    }

    for input in [
        "build/runtime.txt",
        "build/controls.txt",
        "build/extras.rdl",
        "build.rs",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }

    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let output = out_dir.join("bindings.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        // The library is `#![cfg(target_os = "windows")]`, so the include is
        // stripped on every other target; an empty file keeps it cheap.
        std::fs::write(&output, "// not a Windows target\n").expect("write stub bindings.rs");
        return;
    }

    // The pinned package content is immutable, so a hash-verified copy in
    // OUT_DIR satisfies every later build in this target dir.
    let nupkg_cache = out_dir.join(format!(
        "Microsoft.WindowsAppSDK.Runtime-{}.nupkg",
        bindgen::WASDK_VERSION
    ));
    let nupkg = std::fs::read(&nupkg_cache)
        .ok()
        .filter(|cached| bindgen::verify_nupkg(cached))
        .unwrap_or_else(|| {
            let bytes = bindgen::download_nupkg().expect("WASDK nupkg download");
            std::fs::write(&nupkg_cache, &bytes).expect("cache WASDK nupkg");
            bytes
        });

    let winmd_dir = out_dir.join("winmd");
    bindgen::extract_winmds(&nupkg, &winmd_dir).expect("extract WASDK winmds");
    bindgen::compile_extras(
        &manifest.join("build/extras.rdl"),
        &winmd_dir.join("extras.winmd"),
    )
    .expect("compile extras.rdl");
    bindgen::generate(
        &winmd_dir,
        &[
            manifest.join("build/runtime.txt"),
            manifest.join("build/controls.txt"),
        ],
        &output,
    );
}
