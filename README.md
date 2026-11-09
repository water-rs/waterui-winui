# waterui-winui

The WinUI 3 backend for [WaterUI](https://github.com/water-rs/waterui). It renders a WaterUI view tree with native WinUI 3 controls through the Windows App SDK. It is an experimental backend.

## Building

The crate builds only for Windows targets. Its build script generates the WinRT bindings into `OUT_DIR` with windows-bindgen.

- **rustfmt.** The toolchain needs the `rustfmt` component (`rustup component add rustfmt`). windows-bindgen always formats its output by running `rustfmt`, and the build fails without it. The requirement goes away once windows-bindgen can skip formatting (microsoft/windows-rs#5030; tracked in #97).
- **Network on the first build.** The build script downloads the pinned `Microsoft.WindowsAppSDK.Runtime` 2.4.0 package from nuget.org and verifies its SHA-256 to read the Windows App SDK metadata. Later builds reuse the verified package from the build directory.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
