# ALVR (fork) — agent instructions

This is a fork of [ALVR](https://github.com/alvr-org/ALVR), streaming VR from PC to headset. Rust workspace (`cargo xtask` build system) plus an Android/OpenXR client. This fork adds Huawei VR (HVR) and QiYu SDK support to the Android client — **do not remove or "fix" HVR/QiYu code paths as if they were upstream cruft.**

## Hard constraints

- Version is pinned to **20.14.0** in [Cargo.toml](Cargo.toml) (`workspace.package.version`). Do not bump it.
- Build and test only within this checkout. Do not touch or reference other local ALVR checkouts (e.g. a v21 tree) — this repo is intentionally frozen at 20.14.0.
- Never build/copy artifacts to network deploy paths; the user deploys manually.
- Do not modify Desktop2Stereo integration code if present.
- The Android APK must keep working for both HVR and QiYuVR hardware paths.

## Build

Everything goes through `cargo xtask` (see [alvr/xtask](alvr/xtask/src/main.rs) for full subcommand/flag list, or run `cargo xtask --help`). Full instructions: [wiki/Building-From-Source.md](wiki/Building-From-Source.md).

```powershell
cargo xtask prepare-deps --platform windows   # first time / after deps change
cargo xtask build-streamer --release          # -> build/alvr_streamer_windows
cargo xtask build-streamer-openxr             # standalone OpenXR runtime, no SteamVR
cargo xtask build-client --platform android   # -> build/alvr_client_android (merges Qiyu/HVR Java libs into the APK)
cargo xtask format / check-format / clippy
```

Other scripts at repo root:
- [build-controller.ps1](build-controller.ps1) — build + unit-test controller pose / grip-to-aim calibration.
- [test-hello-d3d11.ps1](test-hello-d3d11.ps1) / [test-hello-opengl.ps1](test-hello-opengl.ps1) — run OpenXR `hello_xr` against this ALVR runtime, log to `test-logs/`.
- [capture-hvr-log.ps1](capture-hvr-log.ps1) — pull filtered `adb logcat` (HVR-DIAG, hvrsdk, OpenXR, ALVR native) to `test-logs/hvr-logcat.txt`.

## Architecture (crates under `alvr/`)

Server-side: `server_core` (C-ABI server logic) → `server_openvr` (SteamVR driver) or `server_openxr` (standalone runtime) → `server_io` (storage/system info, shared with `dashboard`).

Client-side: `client_core` (main client logic, C-ABI) → `client_openxr` (OpenXR entry point; contains the Qiyu/HVR C++ bindings, see `src/qiyu_backend.cpp` and `build.rs`) → `client_mock` (headless simulator for desktop testing).

Shared: `packets`/`sockets` (network protocol), `session` (settings schema), `graphics` (glow/OpenGL utils), `common`, `events`, `filesystem`, `system_info`. `gui_common`/`dashboard`/`launcher` are the desktop UI (egui). `vulkan_layer` and `vrcompositor_wrapper` are Linux-only compositor interop. `adb` wraps Android Debug Bridge for install/log flows. For legacy QiYu SXR reference behavior, the indexed checkout `C:\Users\xul\Downloads\ALVR-QIYU` is the source of truth — search it (CodeGraph) instead of guessing at SDK semantics.

## Code style

Rust style conventions (naming, struct/impl ordering, error handling, comments) are documented in [CONTRIBUTING.md](CONTRIBUTING.md) — read it before writing Rust code in this repo.
