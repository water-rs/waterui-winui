# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/water-rs/waterui-winui/compare/v0.0.0...v0.1.0) - 2026-09-20

### Other

- make the nightly gate informational for this experimental backend ([#66](https://github.com/water-rs/waterui-winui/pull/66))
- adopt the 0.5.0 framework wave (0.1.0) ([#64](https://github.com/water-rs/waterui-winui/pull/64))
- publish on the main push, not under workflow_run ([#63](https://github.com/water-rs/waterui-winui/pull/63))
- repin waterui to f004c2fd
- adopt waterui bed5ec50 (GpuContext carries a DeviceLoss handle)
- check the framework out at the pinned revision
- adopt the 0.5.0 framework wave
- Update the backend to the WaterUI 0.4.x framework ([#58](https://github.com/water-rs/waterui-winui/pull/58))
- Activate the self-contained manifest from the DLL for CEF runners ([#55](https://github.com/water-rs/waterui-winui/pull/55))
- dispatch-time example filter and CEF stderr logging ([#50](https://github.com/water-rs/waterui-winui/pull/50))
- *(nightly)* warm the shared release target once instead of per example ([#53](https://github.com/water-rs/waterui-winui/pull/53))
- gate pull requests into main on a green nightly at the head commit
- Fix CEF self-contained detection and dialog false-positive paint ([#48](https://github.com/water-rs/waterui-winui/pull/48))
- Install the dispatcher executor before building the App ([#44](https://github.com/water-rs/waterui-winui/pull/44))
- skip PDB generation for libcef-linked CEF runners ([#42](https://github.com/water-rs/waterui-winui/pull/42))
- disable debug info and incremental artifacts
- keep a bin target on CEF runners
- pass module handle out-param as a raw pointer
- run CEF examples through the bootstrap launcher
- run Effect::setup before first encode_render ([#36](https://github.com/water-rs/waterui-winui/pull/36))
- enable cef-runtime and stage the CEF distribution for CEF examples ([#37](https://github.com/water-rs/waterui-winui/pull/37))
- use IProgressRing setters, not RangeBase ([#34](https://github.com/water-rs/waterui-winui/pull/34))
- Bridge WaterUI video onto WinUI MediaPlayerElement ([#32](https://github.com/water-rs/waterui-winui/pull/32))
- drop OrderedDictionary.Clone — build metric records inline
- fix release benchmark crash and isolate it from the summary
- record release size, memory, and startup latency
- Fix Arc translation in filled shape paths
- Clip custom paths through a composition geometric clip
- expose Direct2D geometry sinks and composition path interop
- Skip zero-size captures in the AppliedFilter pump ([#20](https://github.com/water-rs/waterui-winui/pull/20))
- Fix stretch-axis measurement collapsing TabView content ([#19](https://github.com/water-rs/waterui-winui/pull/19))
- run tests with cargo nextest ([#18](https://github.com/water-rs/waterui-winui/pull/18))
- Repair truncated msix in the staging cache, not just missing files
- Raise captured window to top and enumerate process windows on timeout ([#15](https://github.com/water-rs/waterui-winui/pull/15))
- Stop calling UIElement::Measure inside the arrange pass ([#14](https://github.com/water-rs/waterui-winui/pull/14))
- Box list indices as IInspectable and log unhandled XAML exceptions ([#13](https://github.com/water-rs/waterui-winui/pull/13))
- Virtualize List and fix nightly-exposed render bugs ([#12](https://github.com/water-rs/waterui-winui/pull/12))
- Fix backend bugs and capture fidelity exposed by nightly runs ([#11](https://github.com/water-rs/waterui-winui/pull/11))
- repair self-contained runtime staging beside runner exes ([#10](https://github.com/water-rs/waterui-winui/pull/10))
- Embed self-contained runtime manifest in generated runner crates ([#9](https://github.com/water-rs/waterui-winui/pull/9))
- Check out waterui submodules and keep harness exit code green ([#8](https://github.com/water-rs/waterui-winui/pull/8))
- Fix nightly runner parse error and window-handle race ([#7](https://github.com/water-rs/waterui-winui/pull/7))
- Fix foreach-statement inside subexpression in nightly runner ([#6](https://github.com/water-rs/waterui-winui/pull/6))
- Add nightly CI that screenshots every waterui example ([#5](https://github.com/water-rs/waterui-winui/pull/5))
- Add form example binary and CI screenshot artifact ([#3](https://github.com/water-rs/waterui-winui/pull/3))
- Implement native WinUI 3 backend for WaterUI ([#1](https://github.com/water-rs/waterui-winui/pull/1))
- Bootstrap repository: license files and gitignore
