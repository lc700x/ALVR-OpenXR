# Huawei VR (HVR) porting notes

What it took to make the ALVR OpenXR client work on Huawei VR Glass (`ELS-AN00` / `HWELS`), and
what to check first when it breaks again. Everything here was established from device captures, not
from the OpenXR specification: Huawei's runtime is a pre-1.0 legacy implementation that departs from
the spec in several places, and the official sample is the only reliable statement of what it
actually expects.

**The official sample is the specification.** When Huawei's behaviour and the OpenXR spec disagree,
follow the sample:

```
build/temp_download/vr-sdks/HVRsdkDemo/HVRSDK_XrDemo/jni/xr_demo.cpp
```

## Hard rule: one APK, many headsets

A single APK serves Quest, Pico, Vive, YVR, QiYu and HVR. Every vendor fix must be gated
(`platform == Platform::Hvr` / `is_hvr`) or be provably identical for everyone else. State which
devices a change touches before making it.

## What Huawei's runtime does differently

| Area | Huawei's behaviour | Consequence |
|---|---|---|
| Environment blend mode | Only `ALPHA_BLEND` is offered, and there is no passthrough camera | A layer with sub-unity alpha composites against black |
| `xrEnumerateSwapchainImages` | Returns zeros at creation; one GL handle is filled in per image as that image is first acquired | Render targets cannot be built until several frames in |
| Swapchain image calls | Kill the process if made outside a frame | No setup work between `xrEndFrame` and the next `xrBeginFrame` |
| EGL context | The runtime binds its own context on the render thread inside the swapchain calls | GL commands afterwards go to the wrong context unless rebound |
| `eglMakeCurrent` from another thread | Fails with `EGL_BAD_ACCESS` | Treat as non-fatal, do not panic |
| `xrGetReferenceSpaceBoundsRect` | Returns an error | Never `unwrap()` it |
| Reference spaces | `LOCAL`, `STAGE` and `LOCAL_FLOOR` are all accepted, but a space is anchored to the head pose in effect *when it is created*, and `STAGE` only comes out floor-anchored if it is created after the session is running | Two spaces created at different moments face different directions; a `STAGE` created at setup behaves like `LOCAL` |
| `xrLocateViews` | Ignores the reference space it is given and returns the head at that space's origin | The head never translates, whatever space is passed; the real pose has to come from a `VIEW` space relation |
| Controller aim pose | Floor-referenced, in the same frame as a floor-anchored `STAGE` space | Correct once `STAGE` is used; against `LOCAL` it reads one eye height high |
| `xrEndFrame` and the swapchain calls | Return non-success codes but keep working; the sample ignores them | Tolerate errors instead of panicking, but count them |
| `ImageReader` | `maxImages` is 3, against a default of 10 | The decoder queue must be clamped or it dies with `MaxImagesAcquired` |
| logd | Drops `info`/`warn` records while the vendor SDK floods the buffer | HVR diagnostics must be logged at `error` level |

## The fixes, in the order they were found

Each is a commit on `qiyu-vractivity-wip`; the commit body explains the reasoning in full.

### 1. Dark screen at launch — ambiguous launcher intent

`dumpsys` showed both `VRActivity` and `NativeActivity` resolving
`com.huawei.android.vr.action.MAIN`. The launcher reported "start activity success" and no process
started. The QiYu manifest rewrite must strip the `NativeActivity` intent filters.

The original implementation matched manifest text containing `\n`, but apktool writes CRLF, so it
silently matched nothing. It now parses element ranges and asserts the expected number of removals
(`assert_eq!(removed_filters, 2, ...)`) so a silent no-op becomes a build failure.

*`alvr/xtask/src/build.rs`*

### 2. Frame freeze after ~27 s — decoder queue exceeded the ImageReader

`ImageReader error: MaxImagesAcquired`. Huawei's `maxImages` is 3 where the default is 10, and
`max_buffering_frames: 2.0` produced a queue bound of 4.

```rust
let max_queued_images = super::decoder_max_images().max(2) as usize - 1;
let available_buffering_frames =
    ((2. * config.max_buffering_frames).ceil() as usize).min(max_queued_images);
```

*`alvr/client_core/src/video_decoder/android.rs`*

### 3. Flash then black — the projection layer asked for alpha blending

`XR_COMPOSITION_LAYER_BLEND_TEXTURE_SOURCE_ALPHA_BIT` against `ALPHA_BLEND` with no passthrough
camera composites to black. `HVRSDK_XrDemo` submits `layerFlags = 0` and never consults
`frameState.shouldRender`; do both on HVR.

*`alvr/client_openxr/src/lobby.rs`, `stream.rs`, `lib.rs`*

### 4. Intermittent freeze / black — the runtime stole the EGL context

Huawei binds its own EGL context inside the swapchain acquire/wait calls, so every GL command the
stream renderer issued afterwards went to the wrong context. The lobby already called
`ensure_hvr_current()` after its wait; the stream never did. Whether the runtime takes the context
varies per session, which is exactly why the symptom was intermittent.

*`alvr/client_openxr/src/stream.rs`, `alvr/graphics/src/stream.rs`*

### 5. Streamed world faced the wrong direction — two reference spaces

Huawei re-anchors a `LOCAL` space to the head pose in effect *when the space is created*. The lobby
created its space at session setup and faced forward; the stream created its own when streaming
started and faced wherever the user happened to be looking. `HVRSDK_XrDemo` creates its `LOCAL`
space once and reuses it.

The space is now created once in `lib.rs` and passed to both the lobby and the stream, and neither
recreates it on `ReferenceSpaceChangePending`.

*`alvr/client_openxr/src/lib.rs`, `lobby.rs`, `stream.rs`*

### 6. Mid-session crash — `xrGetReferenceSpaceBoundsRect` unwrapped

Huawei returns an error from it, and it is called again on every `ReferenceSpaceChangePending`
event. `.ok().flatten()` — a missing playspace is already a supported state.

*`alvr/client_openxr/src/stream.rs`*

### 7. Controller pose and bindings

- Interaction profile: `/interaction_profiles/huawei/6dof_controller`
- Grip-to-aim offset is the exact inverse of the VDXR calibration in `手柄方位标定说明.md`:
  position `(0.0, -0.03, 0.095)`, rotation `+60°` about X.
- Per-hand subaction paths are required; a shared path reports both hands at the same pose.

*`alvr/client_openxr/src/interaction.rs`*

### 8. Controllers floated at head height — the wrong reference space, then a second bug under it

The hands sat about 1.5 m above where they were held while the views were correct. Two runtime
quirks stacked up, and the first fix only cancelled the symptom:

- **ALVR asked for the wrong space.** It created `LOCAL` and pushed it down by `1.5 m` so the floor
  would land under the user's feet, because HVR was believed to support only `LOCAL`. The runtime
  accepts `STAGE` and `LOCAL_FLOOR` as well, and `STAGE` is floor-anchored with the real eye height
  (`1.36-1.50 m`) and a real 6DoF head pose. Against `LOCAL` the aim pose — which is floor-referenced
  — reads one eye height high, which is where the 1.5 m subtraction came from.
- **`xrLocateViews` ignores the space.** It returns the head at the origin of whatever space it is
  given, so even with `STAGE` the head stayed put and the hands kept sliding when the headset moved.
  The pose has to come from `xrLocateSpace(VIEW, space)`, which returns the real floor-relative pose.

Measured on device (ELS-AN00, HVRSDK 3.5.0.79) in the same frame, with `STAGE` as the app space:

```
views (xrLocateViews)      = (0.001, 1.399, 0.000)   <- only the space's origin
related (VIEW -> STAGE)    = (0.000, 1.399, 0.000)   <- the real head pose
left controller (aim)      = (-0.278, 0.952, -0.403) <- floor-referenced, same frame
```

Two further details, both measured:

- A space is anchored to the head pose in effect **when it is created**. A `STAGE` created at session
  setup comes out head-anchored (head y ~ 0); one created once the session is running comes out
  floor-anchored. ALVR therefore creates it on the first running frame and installs it into the lobby
  and the stream.
- The controllers are rotation-stable: turning the head ~68 deg moved the aim pose 6 mm
  (`(-0.288, 2.447, -0.308)` to `(-0.282, 2.451, -0.306)`), so nothing in the controller path needed
  to change.

The controller eye-height subtraction (`fix_hvr_controller_pose`) and a later head pin
(`pin_hvr_head_views`) were both attempts to compensate for the wrong space. Both are gone: with
`STAGE` and the `VIEW` relation the poses line up as they are, and this matches the official sample,
which locates the `AimPose` action spaces in the app space and uses the result verbatim
(`jni/xr_demo.cpp` 349-356 and 920-932; `xr_demo.h:174` binds `AimPose` to `aim/pose`, the path ALVR
binds for HVR).

*`alvr/client_openxr/src/interaction.rs`, `lobby.rs`, `stream.rs`, `lib.rs`*

## Still open

A single visual glitch when the stream takes over from the lobby. Instrumentation
(`[HVR-PROBE]`, `alvr/client_openxr/src/stream.rs` and `lib.rs`) has cleared:

- `xrEndFrame` — 4200 frames, zero errors
- Submitted layer count — one throughout
- Swapchain image index — rotates `0 → 1 → 2 → 3` correctly
- `render_targets_ready` — true from the first frame
- Frame time — never above 50 ms, so it is not a stall

Two approaches were tried and reverted, both made it worse:

- **Swapchain priming** (cycling every image at creation to fill the handles in one go) crashed the
  process, because Huawei kills the app for swapchain calls made outside a frame. Moved inside the
  first frame it cost 291 ms of blocking wait, which read as a freeze.
- **Presenting the lobby until the first decoded video frame arrives** never showed the lobby at all
  and turned the freeze into a longer black period. Rendering both layers in one frame appears to
  upset the runtime.

The one measurement not yet explained: roughly **20 seconds** pass between `Stream starting` and
`decoded video frame 1`. That is a server- or network-side delay, not a compositor problem, and it
is the most promising thread to pull next.

## Capturing logs from the headset — read this before debugging

Two traps cost hours each.

**1. `logcat TAG:S ... '*:V'` does not filter anything.** A trailing `*:V` overrides every preceding
`TAG:S` suppression, so the command captures everything. The vendor SDK emits thousands of lines a
second (`CtrlCpp: Controller.cpp:controllerStatusCallBack`), which evicts ALVR's own lines from even
a 64 MB buffer within minutes. This is why diagnostics appeared to "never fire" when they were in
fact printing.

**2. Huawei's logd drops `info` and `warn` records under that load.** Five `info!` lines vanished
from a capture that kept every `error!` line logged in the same 47 ms window. Any HVR diagnostic
that has to survive a capture must be `error!`.

The working method filters *on the device* and writes to the device, so neither the log spam nor a
dropped adb connection can truncate the capture:

```powershell
.\capture-alvr-log.ps1 start    # then reproduce on the headset
.\capture-alvr-log.ps1 stop     # pulls to logs\alvr.txt
```

Sample **on events, not on a schedule**. A periodic probe every 60th frame missed the glitch
entirely, and — because 60 is a multiple of the four-image swapchain — made the acquired image index
look stuck at 0 when it was rotating perfectly. Report on change: first frames, frames slower than a
threshold, transitions in a boolean of interest, and every error.

## PowerShell pitfalls

- `&` at the start of a line is a parse error. Assign the path to a variable first.
- Quote `'*:V'` or PowerShell mangles it.
- Do not nest `powershell -Command "...$(Expr)..."` with double quotes inside a PowerShell session:
  the outer shell interpolates `$(...)` before the child runs, so verification echoes report stale
  state. This looks exactly like "cwd doesn't persist" but is a quoting bug.
- Restarting the adb daemon drops the wireless connection and kills any running capture.
- Two devices are often attached at once. Always pass `-s <serial>`.
