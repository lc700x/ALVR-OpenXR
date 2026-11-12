# Restoring this build on another machine

This archive is a fork of [ALVR](https://github.com/alvr-org/ALVR) pinned to **20.14.0**, with
Huawei VR (HVR) and QiYu support added to the Android/OpenXR client. It contains the full source
tree, the vendor SDK pieces the build reads directly, and the complete git history.

Nothing in it references a user account or an absolute path.

## Contents

| Path | What it is |
|---|---|
| `alvr/`, `wiki/`, `Cargo.toml`, … | The source tree, exactly as tracked in git |
| `alvr-history.bundle` | The full git history, including every fix commit and its reasoning |
| `build/temp_download/vr-sdks/QiyuNativeSDK-v1.0.0/QiyuNativeSDK/` | QiYu `Include` and `Lib` — read by `alvr/client_openxr/build.rs` and by `xtask` |
| `build/temp_download/vr-sdks/HVRsdkDemo/` | Huawei's official OpenXR sample. Supplies `hvrbridge.jar`, and `HVRSDK_XrDemo/jni/xr_demo.cpp` is the reference for how the runtime actually behaves |
| `build/tools/apktool.jar` | Rewrites the binary `AndroidManifest.xml` so `VRActivity` becomes the launcher |

Not included, and why:

- `openvr/` — a git submodule; restore it from upstream (below). Only needed for the SteamVR driver.
- `deps/` — fetched by `cargo xtask prepare-deps`.
- `target/`, `build/alvr_client_android/qiyu_java/` — build output, regenerated every build.
- `build/alvr_client_android/debug.keystore` — `xtask` creates one if it is missing. The new key will
  differ from the old, so **uninstall the existing APK from the headset before the first install**.
- `build/temp_download/vr-sdks/QiyuNativeSDK-v1.0.0/Samples/` — 100 MB of samples the build does not read.

## Setup

Prerequisites: Rust (with the `aarch64-linux-android` target), the Android SDK with build-tools
32.0.0 and platform android-32, and a JDK. Set `ANDROID_HOME` and `JAVA_HOME`.

```powershell
# 1. Restore the git repository from the bundle (keeps all history)
git clone alvr-history.bundle alvr-openxr-v20
# then copy build/temp_download and build/tools from this archive into the clone

# 2. Restore the OpenVR submodule (only needed for build-streamer)
git submodule update --init --depth 1

# 3. Fetch the remaining dependencies
cargo xtask prepare-deps --platform windows
```

If you would rather not use git, the extracted tree is already complete — skip step 1.

## Building

```powershell
cargo xtask build-client --platform android --release   # -> build/alvr_client_android/qiyu_java/alvr_client_qiyu_signed.apk
cargo xtask build-streamer --release                    # SteamVR driver
cargo xtask build-streamer-openxr                       # standalone OpenXR runtime, no SteamVR
cargo xtask format / check-format / clippy
```

The client build takes 2–5 minutes. The resulting APK is universal: one binary serves Quest, Pico,
Vive, YVR, QiYu and HVR.

## Debugging on a headset

```powershell
$env:ALVR_DEVICE = '<headset-ip>:5555'
.\capture-alvr-log.ps1 start    # then reproduce on the headset
.\capture-alvr-log.ps1 stop     # pulls to logs\alvr.txt
```

Read `wiki/Huawei-VR-Porting-Notes.md` before debugging anything HVR-related. It documents where
Huawei's runtime departs from the OpenXR specification, the fixes that came out of each departure,
two approaches that were tried and made things worse, and the two logging traps that make
diagnostics silently disappear.

## Ground rules carried over

- The version is pinned to 20.14.0 in `Cargo.toml`. Do not bump it.
- One APK serves every headset. Gate vendor fixes behind the platform check, or make them provably
  identical for everyone else.
- Huawei's official sample is the specification. Where it and the OpenXR spec disagree, follow the
  sample.
- Do not modify the Desktop2Stereo integration.
