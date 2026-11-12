# Handoff: Huawei VR Controller Height

Date: 2026-11-12

## Goal and constraints

Correct the Huawei VR (HVR) 6DoF controllers appearing above the head in ALVR. Preserve controller movement and do not affect QiYu or other device paths. The change is committed but must not be released or pushed until the user explicitly approves.

## Current result

Commit `068c22f0` (`fix(client-openxr): correct HVR controller initial height`) adds an HVR-only initial Y reflection. For each hand, the first valid controller pose is reflected vertically across the initial head height; subsequent raw controller Y deltas are preserved. X, Z, orientation, and movement deltas are not inverted. The helper is used by HVR lobby and streaming paths. `[HVR-DIAG]` logs report calibration and lobby pose values.

The corresponding engineering record is in `issue.md` in this directory.

## Evidence

A live sample collected before the headset was disconnected, at 2026-11-12 02:42:22, reported:

- Head Y: 1.4570 m and 1.4524 m; average 1.4547 m.
- Left controller Y: 2.4282 m; reflected initial Y: approximately 0.4812 m.
- Right controller Y: 2.3836 m; reflected initial Y: approximately 0.5258 m.
- Runtime logs reported both 6DoF controllers present, `use6DoFPose:true`, `gis6dofposeReady:true`, and `getTrackedPose AIM_POSE`.

The sample supports the vertical-offset correction, but does not prove that the new build behaves correctly in a live stream.

## Validation completed

- `cargo test -p alvr_client_openxr --lib`: 7 passed.
- `cargo xtask build-client --platform android`: succeeded using the installed Android SDK and NDK, supplied only to that shell invocation.
- APK: `F:\alvr-openxr-v20\target\debug\apk\alvr_client_openxr.apk`.
- The headset was disconnected at the user's direction. The new APK was not installed and no post-fix device or visual verification has been done.
- A prior `adb install -r` attempt failed with `INSTALL_FAILED_UPDATE_INCOMPATIBLE` due to a signing-certificate mismatch. The current APK signer has not been compared with the installed app's signer.
- No uninstall, app-data clear, push, or release was performed.

## Next steps when the headset reconnects

1. Confirm the connected ADB device is the Huawei headset before any device mutation.
2. Compare the built APK's signing certificate with the installed package. If they do not match, stop; do not uninstall, clear data, or replace the package using a different key.
3. If signer-compatible, install over the existing package with replacement mode, without uninstalling or clearing data.
4. Launch ALVR and collect fresh `[HVR-DIAG]` calibration and pose logs. Confirm corrected controller Y values are below the head by approximately the sampled offset and remain responsive to vertical movement.
5. Verify the controller position in the active stream visually as well as in logs. Lobby values alone are insufficient to establish the streaming result.
6. Record the device result in `issue.md`. Do not push, publish, or release until the user says OK.

## Repository cautions

- Checkout: `F:\alvr-openxr-v20`, branch `qiyu-vractivity-wip`, ALVR version pinned to 20.14.0.
- The worktree is not clean and contains numerous unrelated pre-existing modifications and deletions. Preserve them; do not use reset/checkout or broad staging.
- `alvr/client_openxr/src/stream.rs` has additional unstaged changes beyond the HVR height fix committed above. Inspect and preserve them before editing or staging that file again.
- The commit contains only `.scratch/huawei-controller-position/issue.md`, `alvr/client_openxr/src/lobby.rs`, and the selected HVR-only hunks in `alvr/client_openxr/src/stream.rs`.
- Keep QiYu and all non-HVR paths unchanged; build only in this checkout. Do not deploy artifacts to network paths.
