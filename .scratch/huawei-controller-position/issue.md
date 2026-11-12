# Huawei Controller Position

Date: 2026-11-12
Status: fixed HVR-only by using STAGE and the VIEW-space relation; built (release), installed and
confirmed on device; not released

## Problem

On Huawei VR (ELS-AN00, HVRSDK 3.5.0.79) the 6DoF controllers were reported roughly 1.5 m above
their real height, so the virtual hands floated around the user's head. After that was patched the
hands still looked like they moved with the headset: hold one still and move your head, and the hand
slid away.

There were two bugs, the second hidden underneath the first.

## Root cause

1. **ALVR asked for the wrong reference space.** HVR was believed to support only `LOCAL`, so
   `get_hvr_floor_reference_space()` created `LOCAL` and pushed it down by 1.5 m to put the floor
   under the user's feet. The runtime accepts `STAGE` and `LOCAL_FLOOR` too, and `STAGE` is
   floor-anchored: the eye reads its real 1.36-1.50 m height and the head pose translates. The aim
   pose is floor-referenced, so against `LOCAL` it is one eye height high.
2. **`xrLocateViews` ignores the requested space.** It returns the head at the origin of whatever
   space it is given. With `LOCAL` that looked correct by accident; with `STAGE` it keeps the head
   at zero while the controllers sit at their real heights, which is what made the hands slide when
   the headset moved. The real pose comes from `xrLocateSpace(VIEW, space)`.

## Evidence (measured on device)

Same frame, `STAGE` as the app space:

```
views (xrLocateViews)      = (0.001, 1.399, 0.000)
related (VIEW -> STAGE)    = (0.000, 1.399, 0.000)
left controller (aim)      = (-0.278, 0.952, -0.403)
```

Supporting measurements:

- **Space availability**: `create_reference_space` succeeded for `STAGE`, `LOCAL_FLOOR`, `LOCAL` and
  `VIEW` alike, so the "only LOCAL" assumption was wrong.
- **Creation time matters**: a `STAGE` created at session setup came out head-anchored (eye y ≈ 0 in
  it); one created once the session is running came out floor-anchored (eye y = 1.36-1.50).
- **The aim pose is rotation-stable**: turning the head ~68° moved it 6 mm
  (`(-0.288, 2.447, -0.308)` → `(-0.282, 2.451, -0.306)`), so nothing in the controller path needed
  changing.
- **The runtime has a real 6DoF head**: logcat shows
  `isHW6DoFHeaderAvailable true` and helmet poses with `use6DoFPose:true, gis6dofposeReady:true` at
  a real eye height (`y ≈ 1.467`). It was ALVR's space choice, not the hardware, that made the head
  look 3DoF.

## Change

- `interaction::get_hvr_floor_reference_space()` creates `STAGE` with an identity pose.
- The space is created on the first *running* frame (`lib.rs`) and installed into the lobby and the
  stream, because a space created earlier is anchored to the head.
- `Lobby::set_hvr_reference_space()` also keeps a `VIEW` space; the lobby replaces the eye poses
  with `view_space.relate(stage)`.
- `stream.rs` does the same for its `current_headset_views`, and `get_head_data()` overwrites the
  eye poses from the same relation so the reported view offsets match the head.
- Removed: the 1.5 m controller subtraction (`fix_hvr_controller_pose`, `HVR_EYE_HEIGHT_METERS`) and
  a head pin (`pin_hvr_head_views`) that were both compensating for the wrong space. Nothing in
  `get_hand_data` is platform-specific.
- `tracking_reference_space_type()` now returns `STAGE` for every platform, including HVR.

## Validation

- `cargo test -p alvr_client_openxr --lib`: 6 passed.
- `cargo xtask build-client --platform android --release` succeeded (NDK `29.0.14206865`, supplied
  only for that invocation). Release builds are re-signed with
  `build/alvr_client_android/build.keystore`'s debug key (pass `alvrclient`, cert SHA-256
  `9f637dd5…`), which matches the installed package, so `adb install -r` works; debug builds use
  `~/.android/debug.keystore` and fail with `INSTALL_Failed_UPDATE_INCOMPATIBLE`.
- Installed with `adb install -r` (no uninstall, no data clear) and launched; process stayed up with
  no panic in logcat.
- Confirmed by the user on device: hands at the real hands, floor at the feet, and the hands stay
  put when the head moves.
- No push, release or deploy was performed.

## Open notes

- A `STAGE` space created before the session runs is head-anchored. If the space ever needs to be
  recreated, do it on a running frame, and recreate it for the lobby and the stream together so
  both keep the same origin.
- `xrLocateViews` should be re-tested on a newer HVRSDK: if Huawei fixes it, the `VIEW` relation
  replacements in `lobby.rs`, `stream.rs` and `get_head_data()` can go.
- The earlier eye-height subtraction was not wrong by accident: `LOCAL` plus a -1.5 m offset puts
  the floor in the right place, and the aim pose is floor-referenced, so the two errors cancelled
  for position and only showed up as motion. Anyone re-reading that history should know the
  subtraction was a symptom-level fix, not the model.
