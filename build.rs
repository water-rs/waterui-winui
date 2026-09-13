//! Build script for `waterui-winui`.
//!
//! With the `self-contained` feature, stages the Windows App Runtime next to
//! the produced binaries via `windows-reactor-setup` and embeds the marker
//! manifest that `bootstrap` uses to recognize the staged runtime. The package
//! has a bin target (`src/bin/smoke.rs`) so the `rustc-link-arg-bins`
//! instructions emitted by `as_self_contained` are valid.

fn main() {
    #[cfg(feature = "self-contained")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_reactor_setup::as_self_contained();
    }
}
