# Huawei Controller Position

Date: 2026-11-12
Status: HVR-only initial-height correction built; headset disconnected; device verification pending; not released

## Evidence

- Wireless ADB device `192.168.1.106:5555` was identified as Huawei ELS-AN00; `alvr.client.stable` was running version 20.14.0 when sampled.
- The live HVR lobby sample at 2026-11-12 02:42:22 reported head Y positions `1.4570 m` and `1.4524 m`, left controller Y `2.4282 m`, and right controller Y `2.3836 m`. Average head Y was `1.4547 m`.
- Reflecting each initial controller height across the head height gives left Y `0.4812 m` and right Y `0.5258 m`. This addresses the initial vertical offset without reversing later motion.
- Runtime logs at collection time reported both Huawei 6DoF controllers present, `use6DoFPose:true`, `gis6dofposeReady:true`, and `getTrackedPose AIM_POSE`. An earlier `gis6dofposeReady false` observation was stale relative to the collected sample.
- Huawei's local OpenXR sample (`hvrsdk-openxr-3.5.0.79.zip`) enables `/interaction_profiles/huawei/6dof_controller`, binds one `AimPose` action per controller, and creates controller spaces from that action. The provided Huawei API guide is the official reference.
- A lobby/stream-path mismatch was found: the stream path had initial-height reflection, while the active HVR lobby path was still using raw controller poses.

## Change

- HVR controller Y uses `reflected_initial_y = initial_head_y - (initial_controller_y - initial_head_y)` and then adds the controller's raw Y delta since calibration. Thus the initial height is reflected below the head by the same distance while subsequent vertical movement is preserved.
- The shared helper is applied per hand in both HVR lobby and streaming paths. Only Y is adjusted; X, Z, orientation, and movement deltas are unchanged.
- HVR binds one controller pose action to Huawei's `aim/pose`; a separate HVR aim space is omitted. Failed HVR controller action spaces remain absent rather than being replaced with a `LOCAL` head-origin placeholder, and valid poses require both position and orientation flags.
- QiYu and other non-HVR controller pose paths are not given the height correction.
- One-time calibration and periodic lobby pose diagnostics are emitted under `[HVR-DIAG]` for device verification.

## Validation

- `cargo test -p alvr_client_openxr --lib` passed all 7 tests on 2026-11-12, including the HVR height-reflection/movement-preservation test. Existing unrelated compiler warnings remain.
- `cargo xtask build-client --platform android` succeeded on 2026-11-12 with the installed SDK at `C:\Users\zjuli\AppData\Local\Android\Sdk` and NDK `29.0.14206865`, supplied only to that command invocation.
- Built APK: `target/debug/apk/alvr_client_openxr.apk`. The build output reports signing with `C:\Users\zjuli\.android\debug.keystore`.
- This build was not installed: the headset was disconnected at the user's direction. The correction is not yet visually or runtime-verified on the headset.
- A previous `adb install -r` attempt was rejected with `INSTALL_FAILED_UPDATE_INCOMPATIBLE` due to signer mismatch. The current APK's signer has not been checked against the installed package; verify signing before retrying installation when the device reconnects.
- No uninstall, data clear, push, or release was performed.
