# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/water-rs/winui-backend/compare/v0.1.0...v0.2.0) - 2026-11-14

### Added

- *(window)* apply the window's own icon and its changes
- *(window)* re-apply the window background when it changes
- *(window)* apply the window style and its changes
- window maximize, always-on-top, and attention via WinUI presenter and FlashWindowEx
- *(app)* run apps with no window and honour the last-window policy ([#78](https://github.com/water-rs/winui-backend/pull/78))
- *(drag-drop)* [**breaking**] port drag and drop to typed payloads
- *(scroll)* report ScrollViewer offsets through report_offset
- *(scroll)* adopt ScrollViewParts with the report_offset slot
- *(input)* on_key_press via bubbling KeyDown and TextField on_submit

### Fixed

- satisfy clippy after the framework API port
- follow the framework's ShortcutKey, scroll-request and window-background APIs
- *(window)* apply HWND-dependent properties only while the HWND exists
- *(window)* switch presenters before driving overlapped state, write Minimized back, snap WM_SIZING to resize increments

### Other

- release from main and align winui-backend metadata
- *(deps)* point migrated Batch 1 git dependencies to main ([#121](https://github.com/water-rs/winui-backend/pull/121)) ([#122](https://github.com/water-rs/winui-backend/pull/122))
- track the framework, wgpu and waterkit branches and let the lock record the commits
- Merge remote-tracking branch 'origin/dev' into feat/1340-window-icon
- *(window)* move the open-state watcher out of open_window
- *(deps)* pin vello to the revision waterui dev requires
- *(deps)* pin waterui to the rebased window-API revision
- pin waterui to the scroll-offset API
- [**breaking**] migrate to the nami snapshot API
- repin the vello fork to the glifo canonical-location fix
- gate the declared rust-version on the floor toolchain ([#72](https://github.com/water-rs/winui-backend/pull/72))
- consume the 0.5.0 framework wave from crates.io ([#70](https://github.com/water-rs/winui-backend/pull/70))
