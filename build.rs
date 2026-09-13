//! Build script for `waterui-winui`.
//!
//! With the `self-contained` feature, stages the Windows App Runtime next to
//! the produced binaries via `windows-reactor-setup`, and additionally embeds
//! the marker manifest into example and test targets so self-contained smoke
//! binaries are recognized by `bootstrap`.

fn main() {
    #[cfg(feature = "self-contained")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_reactor_setup::as_self_contained();
        // `as_self_contained` only emits `/MANIFESTINPUT` for bin targets;
        // examples and tests need it too or the self-contained marker check
        // in `bootstrap` cannot see the staged runtime.
        let manifest = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"))
            .join("app.manifest");
        println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-examples=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
