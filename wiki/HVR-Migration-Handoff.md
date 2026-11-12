# ALVR on Huawei VR (HVR) — migration handoff

Everything below is implemented and verified on the device named in §1. It is gated to
`Platform::Hvr` unless a change is explicitly called generic, so Quest, Pico, Vive, YVR and QiYu
paths are untouched.

## 1. Target

| | |
|---|---|
| Headset | Huawei ELS-AN00 (HVR), HVRSDK 3.5.0.79 |
| Client package | `alvr.client.stable`, ALVR 20.14.0 |
| Launch | `am start -a com.huawei.android.vr.action.MAIN -n alvr.client.stable/alvr.client.VRActivity` |
| Refresh | 90 Hz panel; 72 Hz is refused by the runtime |

The runtime is a **legacy Huawei OpenXR implementation**, not a conformant one. Several of the
symptoms below come from that, not from ALVR's renderer.

## 2. What the runtime does differently

| Area | Behaviour | Consequence |
|---|---|---|
| Reference spaces | `LOCAL`, `STAGE`, `LOCAL_FLOOR`, `VIEW` are all accepted, but a space is anchored to the head pose in effect **when it is created**; `STAGE` only comes out floor-anchored if created after the session runs | A space created at setup behaves like `LOCAL`; must be created on a running frame |
| `xrLocateViews` | Ignores the requested reference space and returns the head at that space's origin | Head never translates; real pose must come from `xrLocateSpace(VIEW, space)` |
| Controller aim pose | Floor-referenced, in the same frame as a floor-anchored `STAGE` | Correct with `STAGE`; against `LOCAL` it is one eye height high |
| Controllers are mono | Both eyes sit on the head; per-eye separation is noise | Preserving it injects jitter; both eyes should follow the head |
| `xrEnumerateSwapchainImages` | Zeros at creation; one GL handle filled per acquired image | Render targets must be built in-frame (deferred path in lobby/stream) |
| Swapchain calls | Kill the process if made outside a frame | No setup work between `xrEndFrame` and the next `xrBeginFrame` |
| EGL context | Runtime binds its own context inside swapchain calls | Rebind ours after `wait_image` or GL goes to the wrong context |
| `xrGetReferenceSpaceBoundsRect` | Returns an error | Never `unwrap()` |
| `request_display_refresh_rate` | Rejects rates it does not offer | Never `unwrap()` |
| Stream swapchain format | Plain `RGBA8`, not sRGB | sRGB linearisation must be disabled for it |
| `logd` | Drops `info`/`warn` while the vendor SDK floods the buffer | HVR diagnostics must be logged at `error` level |

## 3. Divergence from upstream ALVR

Checked against `alvr-org/ALVR` tag `v20.14.0` (commit `d7921382`):

- Upstream has **no HVR platform at all** — zero occurrences of `Platform::Hvr`, no Huawei
  interaction profile, no HVR reference-space handling. Everything in §4 that is gated on
  `Platform::Hvr` is fork-only code, not a modification of upstream logic.
- Upstream `client_openxr/src/interaction.rs` is 1181 lines; this fork's is ~1500 before the HVR
  additions, because the fork also carries the QiYu path.
- This fork's own `main` already contains HVR code, so it is not a clean upstream baseline either;
  use the upstream tag if a three-way comparison is ever needed.

The changes that touch code paths shared with other headsets, and why they are inert for them:

| Change | Shared-path effect |
|---|---|
| `get_hand_data` gained `head_pose` and `is_left_hand` | Signature change only; the 3DoF seeding it enables is gated on `Platform::Hvr` |
| View parameters sent every frame | Condition is `platform == Hvr \|\| eye separation changed`, so other platforms keep the old behaviour |
| sRGB correction gated on the swapchain format | New `gl_format_is_srgb` helper; the HVR clause is additive, other platforms keep their previous setting |
| `request_display_refresh_rate` no longer unwrapped | Generic robustness; success path identical |
| Degenerate view-params guard (client and server) | Generic, fires only on non-finite or absurd values |
| `tracking_reference_space_type` returns `STAGE` for every platform | Only HVR changed (it used `LOCAL`); other platforms already returned `STAGE` |



## 4. Changes

### Tracking (`4b51a25d`)
- HVR uses `STAGE` instead of `LOCAL` shifted down 1.5 m. The space is created on the first
  *running* frame and installed into the lobby and the stream so both share one origin.
- The head pose comes from `xrLocateSpace(VIEW, space)` in the lobby render, the stream render and
  `get_head_data`, because `xrLocateViews` ignores the space on this runtime.
- The 1.5 m controller subtraction (`fix_hvr_controller_pose`) and its constant were removed; they
  existed only to compensate for the wrong space.

### Stream output (`fa8f906e`, `70640cf0`)
- View parameters are sent every frame on HVR. The old code sent them only when the eye separation
  changed, which never happens on a single panel, so the server projected with degenerate values
  (`(-5.7e20, 1.33, 5.2e19)`) and the image was black.
- sRGB linearisation is applied only when the swapchain is actually sRGB. HVR's is `RGBA8`, so the
  image came out very dark.
- Both eyes follow the head pose again (the per-eye shift was reverted after it caused stereo
  jitter).

### 3DoF controllers (`51855d8b`)
- A controller reporting orientation only is accepted (`ORIENTATION_VALID`, position optional).
- Such a hand is seeded in front of the head (−0.2 / +0.2 m sideways, 0.35 m down, 0.35 m forward)
  and then tracks orientation only; 6DoF controllers use their reported position unchanged, and one
  that briefly loses position holds the last one.
- HVR advertises 8-bit, no high profile, no foveated encoding, because the phone decodes the stream
  itself and is the throughput limit.

### Stability
- `9f9b9c22` (client) and `13d62a60` (server): degenerate view parameters are rejected — the client
  falls back to the runtime's poses, the server keeps the last good pair in `LOCAL_VIEW_PARAMS`.
- `70640cf0`: a refused display refresh rate no longer panics the client (72 Hz on a 90 Hz panel
  used to kill it).

## 5. Verification

Measured on device, same frame, `STAGE` as the app space:

```
views (xrLocateViews)   = (0.001, 1.399, 0.000)   <- only the space origin
related (VIEW -> STAGE) = (0.000, 1.399, 0.000)   <- the real head pose
left aim pose           = (-0.278, 0.952, -0.403) <- floor-referenced, same frame
```

- Turning the head ~68° moved the aim pose 6 mm, so nothing in the controller path needed changing.
- The brightness fix was confirmed by the user ("brightness ok"); hands sit at the real hands and
  stay put when the head moves ("好了").
- 3DoF and 6DoF both accepted; the policy is covered by
  `hvr_controller_location_accepts_3dof_and_6dof`.
- Client tests: `cargo test -p alvr_client_openxr --lib` → 7 passed.
- Server: `cargo check -p alvr_server_openxr` (needs `LIBCLANG_PATH` set to the NDK's `libclang.dll`).

## 6. Build, install, release

```powershell
$env:ANDROID_NDK_HOME="C:\Users\<user>\AppData\Local\Android\Sdk\ndk\29.0.14206865"
cargo xtask build-client --platform android --release          # -> target/release/apk/alvr_client_openxr.apk
adb -s <ip>:5555 install -r target\release\apk\alvr_client_openxr.apk
adb -s <ip>:5555 shell am start -a com.huawei.android.vr.action.MAIN -n alvr.client.stable/alvr.client.VRActivity
```

- **`adb install -r` kills the running client** — always relaunch, otherwise the headset shows the
  old process or nothing.
- Release builds are signed with `build/alvr_client_android/debug.keystore` (pass `alvrclient`), which
  matches the installed package. Debug builds use `~/.android/debug.keystore` and fail with
  `INSTALL_FAILED_UPDATE_INCOMPATIBLE`.
- The OpenXR server is built with `cargo xtask build-streamer-openxr` (same `LIBCLANG_PATH`).

Releases: `v1.0.2` (source only) and `v1.0.3` (source + client APK) on
`https://github.com/lc700x/ALVR-OpenXR`, branch `qiyu-vractivity-wip`.

## 7. Known limits and next steps

- **Client frame rate**: the loop runs at roughly 30–40 fps against a 90 Hz panel. It is decode- or
  swapchain-bound on the phone; not yet profiled. The `hvr_wait_image` path waits with an infinite
  timeout, so a bounded wait plus a dropped frame is the first thing to try if the profile shows
  stalls.
- **Server throughput**: not optimised beyond the view-params guard. Needs one session with the
  runtime's `PerfScope` output (`end_frame`, Vulkan/GL interop conversion, encode submission) and a
  count of `end_frame` fallbacks to the latest tracking timestamp. Those numbers decide whether the
  next change is the conversion path, the encoder settings HVR actually gets, or frame pacing.
- **Black screen when the OpenXR app is stopped**: the client should return to the lobby on
  `StreamingStopped`; observed to stay black once, not yet captured.
- **Stopping the streamer**: the client keeps the last stream state until the server disconnects.

## 8. Debugging playbook

- Capture the client's own process only — the vendor SLAM/camera logs overflow the ring buffer in
  seconds: `adb logcat --pid=$(adb shell pidof alvr.client.stable)`.
- `logcat TAG:S '*:V'` does not filter (the trailing `*:V` wins); use `--pid` or `-s`.
- HVR diagnostics are `error!` on purpose (see the logd row in §2); do not "tidy" them to `info!`.
- The lobby and stream log `[HVR-DIAG]` / `[HVR-PROBE]` lines; the probe reports the received
  view pose, which is where both the `1e20` and `-3.66e35` failures were caught.
