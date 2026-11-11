# Huawei Controller Position

Date: 2026-11-12
Status: source patch local; Android build succeeds; device install blocked by signing mismatch; not released

## Evidence

- Wireless ADB device `192.168.1.106:5555` is Huawei ELS-AN00; `alvr.client.stable` is running version 20.14.0.
- Current runtime logs report both Huawei 6DoF controllers present, `use6DoFPose:true`, `gis6dofposeReady:true`, and `getTrackedPose AIM_POSE`. An earlier inspection saw `gis6dofposeReady false`; that was not the current state.
- Huawei's local OpenXR sample (`hvrsdk-openxr-3.5.0.79.zip`) enables `/interaction_profiles/huawei/6dof_controller`, binds one `AimPose` action per controller, and creates controller spaces from that action. ALVR bound both grip and aim actions to the same Huawei `aim/pose` component.
- On HVR action-space creation failure, ALVR substituted a `LOCAL` reference space. That space can report valid location flags at the headset origin and be mistaken for a controller pose.

## Change

- HVR binds only the grip action to Huawei's `aim/pose`; its separate aim space is absent, matching the sample's single pose action.
- Failed HVR controller action spaces remain absent instead of becoming a local/head-origin pose. Existing HVR validity checks still require valid position and orientation flags.
- QiYu and non-HVR bindings and fallback behavior are unchanged.

## Validation

- All 6 `alvr_client_openxr` unit tests passed in the prior validation. Existing unrelated compiler warnings remain in `lib.rs`, `stream.rs`, `graphics.rs`, and `lobby.rs`.
- `cargo xtask build-client --platform android` succeeds with the installed Android SDK/NDK when `ANDROID_HOME` and `ANDROID_NDK_HOME` are set for that command only. The APK is signed with `C:\Users\zjuli\.android\debug.keystore`.
- Installing over the running `alvr.client.stable` package with `adb install -r` was rejected as `INSTALL_FAILED_UPDATE_INCOMPATIBLE`: the locally built APK signer does not match the installed package signer.
- No uninstall, data clear, or package replacement occurred. Device runtime validation remains pending until a matching signing key is available or a separate test package can be installed without disturbing the existing app.
- No push or release was performed.
