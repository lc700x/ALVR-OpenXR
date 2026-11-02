<p align="center"> <img width="500" src="resources/ALVR-Grey.svg"/> </p>

# ALVR 20.14.0 — QiYu / Huawei VR fork

Stream VR games from your PC to your headset over Wi-Fi.

This is a fork of [ALVR](https://github.com/alvr-org/ALVR) pinned to **20.14.0**, adding support for
two headsets upstream does not cover:

- **iQIYI QiYu (奇遇) 3 / A091** — OpenXR path with the vendor extensions enabled, plus a Native SDK
  bridge that recovers the full controller button set.
- **Huawei VR (HVR)** — its legacy OpenXR runtime needs hand subaction paths, a pose-only binding
  fallback, and a floor-height reference space.

Everything else, including all upstream headsets and the SteamVR/OpenXR streamer, is unchanged.

## What this fork adds

### QiYu

The headset ships a Monado-based OpenXR runtime alongside the proprietary SXR (`libqiyuapi.so`)
SDK. Neither one alone is enough, so the client uses both:

| Concern | Source | Why |
| --- | --- | --- |
| Display, tracking, foveation | OpenXR runtime | Higher recommended resolution (2160²) and `XR_QIYU_foveation` hardware foveation |
| Controller buttons, axes, haptics | Native SDK bridge | The runtime only ever activates `khr/simple_controller`, which carries just select and menu |
| Passthrough | `ALPHA_BLEND` environment blend mode | QiYu exposes only the undocumented `XR_QIYU_passthrough`, so ALVR's FB/HTC passthrough layer cannot be created |

The controller bridge reads `qiyu_GetControllerData` directly and publishes the values under the
Oculus Touch button ids the server already maps. `qiyu_Init` must run first, and its `QVR_Init`
dereferences globals that only `libqiyivrsdkcore.so`'s `JNI_OnLoad` sets up, so
[VRActivity.java](alvr/client_openxr/android/java/alvr/client/VRActivity.java) loads the SXR
libraries with `System.loadLibrary` before delegating to `NativeActivity`.

A complete SXR rendering backend also lives in
[qiyu_backend.cpp](alvr/client_openxr/src/qiyu_backend.cpp). It works and is kept for reference, but
is disabled: set `VRActivity.USE_SXR_ON_QIYU = true` to use it instead of OpenXR.

### Huawei VR

`Platform::Hvr` branches cover the runtime's deviations from the specification: per-hand subaction
paths on every action, `/interaction_profiles/huawei/6dof_controller` bindings with a pose-only
retry, a `LOCAL` reference space pushed down to floor height, grip-to-aim controller offsets,
thumbstick deadzone and axis correction, and `ALPHA_BLEND` composition.

## Compatibility

| VR headset | Support |
| --- | --- |
| iQIYI QiYu 3 (A091) | :heavy_check_mark: this fork |
| Huawei VR (HVR) | :heavy_check_mark: this fork |
| Quest 1/2/3/3S/Pro, Pico Neo 3/4, Vive Focus 3/XR Elite, YVR, Lynx R1 | :heavy_check_mark: inherited from upstream, untested here |

| PC OS | Support |
| --- | --- |
| Windows 10/11 | :heavy_check_mark: |
| Linux | :warning: inherited from upstream, untested here |

The PC side needs SteamVR (or the standalone OpenXR runtime built from this repo), a GPU with NVENC
or AMF encoding, and a 5 GHz Wi-Fi link to the headset.

## Build

Everything goes through `cargo xtask`. Full instructions:
[wiki/Building-From-Source.md](wiki/Building-From-Source.md).

```powershell
cargo xtask prepare-deps --platform windows   # first time, or after deps change
cargo xtask build-streamer --release          # -> build/alvr_streamer_windows
cargo xtask build-streamer-openxr             # standalone OpenXR runtime, no SteamVR
cargo xtask build-client --platform android   # -> build/alvr_client_android
```

The Android build merges the QiYu and HVR Java libraries into a single APK at
`build/alvr_client_android/qiyu_java/alvr_client_qiyu_signed.apk`. One APK covers every headset:
`VRActivity` inspects `Build.MANUFACTURER` and hands off to the right path.

Helper scripts at the repo root:

- [build-controller.ps1](build-controller.ps1) — build and unit-test the controller pose /
  grip-to-aim calibration.
- [test-hello-d3d11.ps1](test-hello-d3d11.ps1), [test-hello-opengl.ps1](test-hello-opengl.ps1) — run
  OpenXR `hello_xr` against this runtime, logging to `test-logs/`.
- [capture-hvr-log.ps1](capture-hvr-log.ps1) — pull a filtered `adb logcat` to
  `test-logs/hvr-logcat.txt`.

## Architecture

Crates live under `alvr/`.

**Server:** `server_core` (C-ABI server logic) → `server_openvr` (SteamVR driver) or `server_openxr`
(standalone runtime) → `server_io` (storage and system info, shared with `dashboard`).

**Client:** `client_core` (main client logic, C-ABI) → `client_openxr` (OpenXR entry point; also
holds the QiYu C++ bindings) → `client_mock` (headless simulator for desktop testing).

**Shared:** `packets` and `sockets` (network protocol), `session` (settings schema), `graphics`
(glow/OpenGL utilities), `common`, `events`, `filesystem`, `system_info`. `gui_common`, `dashboard`
and `launcher` are the desktop UI (egui). `vulkan_layer` and `vrcompositor_wrapper` are Linux-only
compositor interop. `adb` wraps Android Debug Bridge.

## Diagnostics

The QiYu client tags its log lines so a capture can be read quickly:

| Tag | Meaning |
| --- | --- |
| `[BUILD-ID]` | Which APK the device actually ran. Check this first when a diagnostic is missing. |
| `[CTRL-BRIDGE]` | Native SDK controller bridge: library loading, `qiyu_Init`, live button state |
| `[PROFILE-DIAG]` | Which OpenXR interaction profile the runtime activated per hand |
| `[HVR-DIAG]` | Huawei binding, tracking and frame submission results |

Capture with `adb logcat`; the analysis scripts at the repo root (`parse-hvrlog*.ps1`) filter the
common cases.

## Code style

Rust conventions (naming, struct and impl ordering, error handling, comments) are documented in
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

ALVR is licensed under the [MIT License](LICENSE).

## Upstream

Issues that are not specific to QiYu or HVR belong upstream:
[alvr-org/ALVR](https://github.com/alvr-org/ALVR) · [Discord](https://discord.gg/ALVR) ·
[Wiki](https://github.com/alvr-org/ALVR/wiki)
