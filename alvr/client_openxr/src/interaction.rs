use crate::extra_extensions::MotionTrackerBD;
use crate::{
    extra_extensions::{
        self, BodyJointSetBD, BodyTrackerBD, BodyTrackerFB, EyeTrackerSocial, FaceTracker2FB,
        FaceTrackerPico, FacialTrackerHTC, MultimodalMeta, BODY_JOINT_LEFT_ELBOW_BD,
        BODY_JOINT_LEFT_FOOT_BD, BODY_JOINT_LEFT_KNEE_BD, BODY_JOINT_PELVIS_BD,
        BODY_JOINT_RIGHT_ELBOW_BD, BODY_JOINT_RIGHT_FOOT_BD, BODY_JOINT_RIGHT_KNEE_BD,
        BODY_JOINT_SET_FULL_BODY_META, BODY_JOINT_SPINE3_BD, FULL_BODY_JOINT_COUNT_META,
        FULL_BODY_JOINT_LEFT_FOOT_BALL_META, FULL_BODY_JOINT_LEFT_LOWER_LEG_META,
        FULL_BODY_JOINT_RIGHT_FOOT_BALL_META, FULL_BODY_JOINT_RIGHT_LOWER_LEG_META,
    },
    Platform,
};
use alvr_common::{
    glam::{Quat, Vec3},
    *,
};
use alvr_packets::{ButtonEntry, ButtonValue, StreamConfig};
use alvr_session::{BodyTrackingBDConfig, BodyTrackingSourcesConfig, FaceTrackingSourcesConfig};
use openxr as xr;
use std::{collections::HashMap, time::Duration};
use xr::SpaceLocationFlags;

const IPD_CHANGE_EPS: f32 = 0.001;
const HVR_CONTROLLER_PROFILE_PATH: &str = "/interaction_profiles/huawei/6dof_controller";

fn hvr_binding_path(standard_path: &str) -> Option<String> {
    if standard_path.ends_with("/thumbrest/touch") {
        None
    } else if standard_path == "/user/hand/right/input/system/click" {
        Some("/user/hand/right/input/home/click".into())
    } else {
        Some(standard_path.replace("/squeeze/", "/grip/"))
    }
}

#[cfg(test)]
mod tests {
    use super::{adjust_hvr_thumbsticks, hvr_binding_path};

    use alvr_common::{
        LEFT_THUMBSTICK_X_ID, LEFT_THUMBSTICK_Y_ID, RIGHT_THUMBSTICK_X_ID, RIGHT_THUMBSTICK_Y_ID,
    };
    use alvr_packets::ButtonValue;
    use std::collections::HashMap;

    #[test]
    fn hvr_bindings_match_official_profile() {
        assert_eq!(
            hvr_binding_path("/user/hand/left/input/squeeze/value").as_deref(),
            Some("/user/hand/left/input/grip/value")
        );
        assert_eq!(
            hvr_binding_path("/user/hand/right/input/system/click").as_deref(),
            Some("/user/hand/right/input/home/click")
        );
        assert_eq!(
            hvr_binding_path("/user/hand/left/input/trigger/value").as_deref(),
            Some("/user/hand/left/input/trigger/value")
        );
        assert!(hvr_binding_path("/user/hand/left/input/thumbrest/touch").is_none());
    }

    #[test]
    fn hvr_thumbsticks_match_wolvic() {
        let mut values = HashMap::from([
            (*LEFT_THUMBSTICK_X_ID, (ButtonValue::Scalar(0.10), false)),
            (*LEFT_THUMBSTICK_Y_ID, (ButtonValue::Scalar(0.15), false)),
            (*RIGHT_THUMBSTICK_X_ID, (ButtonValue::Scalar(0.20), false)),
            (*RIGHT_THUMBSTICK_Y_ID, (ButtonValue::Scalar(0.10), false)),
        ]);

        adjust_hvr_thumbsticks(&mut values, true);

        assert!(matches!(
            values.get(&*LEFT_THUMBSTICK_X_ID),
            Some((ButtonValue::Scalar(value), _)) if *value == 0.0
        ));
        assert!(matches!(
            values.get(&*LEFT_THUMBSTICK_Y_ID),
            Some((ButtonValue::Scalar(value), _)) if *value == 0.0
        ));
        assert!(matches!(
            values.get(&*RIGHT_THUMBSTICK_X_ID),
            Some((ButtonValue::Scalar(value), _)) if *value == 0.20
        ));
        assert!(matches!(
            values.get(&*RIGHT_THUMBSTICK_Y_ID),
            Some((ButtonValue::Scalar(value), _)) if *value == -0.10
        ));
    }
}

// Most OpenXR runtime, including Meta's one, do not follow perfectly the specification regarding
// controller pose. The Z axis should point down through the center of the controller grip, the X
// axis should go out perpendicular from the palm, and the position should be aligned roughtly with
// the center of the palm.
// https://registry.khronos.org/OpenXR/specs/1.1/html/xrspec.html
// Note: right controller offsets are calculated from left controller offsets by mirroring along the
// Y-Z plane.
fn get_controller_offset(platform: Platform, is_right_hand: bool) -> Pose {
    const DEG_TO_RAD: f32 = std::f32::consts::PI / 180.0;

    let left_offset = match platform {
        Platform::Quest1 => Pose {
            position: Vec3::new(-0.013, -0.005, 0.0),
            orientation: Quat::from_rotation_x(-20.0 * DEG_TO_RAD),
        },
        // todo: check Quest 2
        p if p.is_quest() => Pose {
            position: Vec3::new(-0.005, -0.005, 0.00),
            orientation: Quat::from_rotation_x(-15.0 * DEG_TO_RAD),
        },
        Platform::PicoNeo3 => Pose {
            position: Vec3::new(-0.013, -0.035, 0.0),
            orientation: Quat::IDENTITY,
        },
        // todo: check (base) Pico 4
        p if p.is_pico() => Pose {
            position: Vec3::new(-0.01, -0.035, 0.0),
            orientation: Quat::from_rotation_y(6.0 * DEG_TO_RAD)
                * Quat::from_rotation_x(-6.0 * DEG_TO_RAD),
        },
        p if p.is_vive() => Pose {
            position: Vec3::new(0.0, 0.0, -0.02),
            orientation: Quat::IDENTITY,
        },
        // HVR exposes only aim/pose. The grip action is bound to that pose;
        // invert VDXR's grip→aim calibration to recover canonical grip.
        Platform::Hvr => Pose {
            position: Vec3::new(0.0, -0.03, 0.095),
            orientation: Quat::from_rotation_x(60.0 * DEG_TO_RAD),
        },
        _ => Pose::default(),
    };

    if is_right_hand {
        let p = left_offset.position;
        let q = left_offset.orientation;

        Pose {
            position: Vec3::new(-p[0], p[1], p[2]),
            orientation: Quat::from_xyzw(-q.x, q.y, q.z, -q.w),
        }
    } else {
        left_offset
    }
}

fn create_ext_object<T>(
    name: &str,
    enabled: Option<bool>,
    create_cb: impl FnOnce() -> xr::Result<T>,
) -> Option<T> {
    enabled
        .unwrap_or(false)
        .then(|| match create_cb() {
            Ok(obj) => Some(obj),
            Err(xr::sys::Result::ERROR_FEATURE_UNSUPPORTED) => {
                warn!("Cannot create unsupported {name}");
                None
            }
            Err(xr::sys::Result::ERROR_EXTENSION_NOT_PRESENT) => None,
            Err(e) => {
                warn!("Failed to create {name}: {e}");
                None
            }
        })
        .flatten()
}

pub enum ButtonAction {
    Binary(xr::Action<bool>),
    Scalar(xr::Action<f32>),
}

pub struct HandInteraction {
    pub controllers_profile_id: u64,
    pub pose_offset: Pose,
    pub subaction_path: xr::Path,

    pub grip_action: xr::Action<xr::Posef>,
    pub grip_space: xr::Space,

    #[expect(dead_code)]
    pub aim_action: xr::Action<xr::Posef>,
    #[expect(dead_code)]
    pub aim_space: xr::Space,

    pub vibration_action: xr::Action<xr::Haptic>,
    pub skeleton_tracker: Option<xr::HandTracker>,
}

pub struct FaceSources {
    pub combined_eyes_source: Option<(xr::Action<xr::Posef>, xr::Space)>,
    pub eye_tracker_fb: Option<EyeTrackerSocial>,
    pub face_tracker_fb: Option<FaceTracker2FB>,
    pub eye_tracker_htc: Option<FacialTrackerHTC>,
    pub lip_tracker_htc: Option<FacialTrackerHTC>,
    pub face_tracker_pico: Option<FaceTrackerPico>,
}

pub struct BodySources {
    pub body_tracker_fb: Option<(BodyTrackerFB, usize)>,
    pub body_tracker_bd: Option<BodyTrackerBD>,
    pub motion_tracker_bd: Option<MotionTrackerBD>,
}

#[derive(Clone)]
pub struct InteractionSourcesConfig {
    pub face_tracking: Option<FaceTrackingSourcesConfig>,
    pub body_tracking: Option<BodyTrackingSourcesConfig>,
    pub prefers_multimodal_input: bool,
}

impl InteractionSourcesConfig {
    pub fn new(config: &StreamConfig) -> Self {
        Self {
            face_tracking: config
                .settings
                .headset
                .face_tracking
                .as_option()
                .map(|c| c.sources.clone()),
            body_tracking: config
                .settings
                .headset
                .body_tracking
                .as_option()
                .map(|c| c.sources.clone()),
            prefers_multimodal_input: config
                .settings
                .headset
                .controllers
                .as_option()
                .is_some_and(|c| c.multimodal_tracking),
        }
    }
}

pub struct InteractionContext {
    xr_session: xr::Session<xr::OpenGlEs>,
    xr_system: xr::SystemId,
    extra_extensions: Vec<String>,
    platform: Platform,
    pub action_set: xr::ActionSet,
    pub button_actions: HashMap<u64, ButtonAction>,
    pub hands_interaction: [HandInteraction; 2],
    pub hvr_position_tracking: bool,
    multimodal_handle: Option<MultimodalMeta>,
    pub multimodal_hands_enabled: bool,
    pub face_sources: FaceSources,
    pub body_sources: BodySources,
}

impl InteractionContext {
    pub fn new(
        xr_session: xr::Session<xr::OpenGlEs>,
        extra_extensions: Vec<String>,
        xr_system: xr::SystemId,
        platform: Platform,
    ) -> Self {
        let xr_instance = xr_session.instance();

        let hvr_position_tracking = if platform == Platform::Hvr {
            let supported = xr_instance
                .system_properties(xr_system)
                .map(|properties| properties.tracking_properties.position_tracking)
                .unwrap_or(false);
            info!("[HVR-DIAG] position tracking supported: {supported}");
            supported
        } else {
            false
        };

        let action_set = xr_instance
            .create_action_set("alvr_interaction", "ALVR interaction", 0)
            .unwrap();

        // Huawei's legacy runtime requires every controller action to declare the same hand
        // subaction path used by its suggested binding and action space. The official HVR sample
        // does this for all actions; using an empty subaction list makes HVR reject every binding
        // with "Rejected! SUB PATH" and leaves the session in an invalid state.
        let hvr_hand_paths = (platform == Platform::Hvr).then(|| {
            [
                xr_instance.string_to_path("/user/hand/left").unwrap(),
                xr_instance.string_to_path("/user/hand/right").unwrap(),
            ]
        });

        let mut bindings = vec![];

        fn binding<'a, T: xr::ActionTy>(action: &'a xr::Action<T>, path: &str) -> xr::Binding<'a> {
            xr::Binding::new(action, action.instance().string_to_path(path).unwrap())
        }

        let controllers_profile_path = match platform {
            p if p.is_quest() => QUEST_CONTROLLER_PROFILE_PATH, // todo: create new controller profile for quest pro and 3
            Platform::PicoG3 => PICO_G3_CONTROLLER_PROFILE_PATH,
            Platform::PicoNeo3 => PICO_NEO3_CONTROLLER_PROFILE_PATH,
            Platform::Pico4Ultra => PICO4S_CONTROLLER_PROFILE_PATH,
            Platform::Pico4 | Platform::Pico4Pro | Platform::Pico4Enterprise => {
                PICO4_CONTROLLER_PROFILE_PATH
            }
            p if p.is_pico() => PICO4S_CONTROLLER_PROFILE_PATH,
            p if p.is_vive() => FOCUS3_CONTROLLER_PROFILE_PATH,
            p if p.is_yvr() => YVR_CONTROLLER_PROFILE_PATH,
            // Reuse Touch button inventory for server-side mapping. Bindings are
            // suggested against Huawei's actual interaction profile below.
            Platform::Hvr => QUEST_CONTROLLER_PROFILE_PATH,
            _ => QUEST_CONTROLLER_PROFILE_PATH,
        };
        let interaction_profile_path = if platform == Platform::Hvr {
            HVR_CONTROLLER_PROFILE_PATH
        } else {
            controllers_profile_path
        };
        let controllers_profile_id = alvr_common::hash_string(controllers_profile_path);

        // Create actions:

        let mut button_actions = HashMap::new();
        for button_id in &CONTROLLER_PROFILE_INFO
            .get(&controllers_profile_id)
            .unwrap()
            .button_set
        {
            let info = BUTTON_INFO.get(button_id).unwrap();

            let name = info.path[1..].replace('/', "_");
            let display_name = format!(
                "{}{}",
                name[0..1].to_uppercase(),
                name[1..].replace('_', " ")
            );
            let subaction_paths = hvr_hand_paths
                .as_ref()
                .map(|paths| {
                    if info.path.starts_with("/user/hand/left/") {
                        &paths[0..1]
                    } else {
                        &paths[1..2]
                    }
                })
                .unwrap_or(&[]);

            let action = match info.button_type {
                ButtonType::Binary => ButtonAction::Binary(
                    action_set
                        .create_action(&name, &display_name, subaction_paths)
                        .unwrap(),
                ),
                ButtonType::Scalar => ButtonAction::Scalar(
                    action_set
                        .create_action(&name, &display_name, subaction_paths)
                        .unwrap(),
                ),
            };
            button_actions.insert(*button_id, action);
        }

        let left_subaction_paths = hvr_hand_paths
            .as_ref()
            .map_or(&[][..], |paths| &paths[0..1]);
        let right_subaction_paths = hvr_hand_paths
            .as_ref()
            .map_or(&[][..], |paths| &paths[1..2]);

        let left_grip_action = action_set
            .create_action("left_grip_pose", "Left grip pose", left_subaction_paths)
            .unwrap();
        let right_grip_action = action_set
            .create_action("right_grip_pose", "Right grip pose", right_subaction_paths)
            .unwrap();

        let left_aim_action = action_set
            .create_action("left_aim_pose", "Left aim pose", left_subaction_paths)
            .unwrap();
        let right_aim_action = action_set
            .create_action("right_aim_pose", "Right aim pose", right_subaction_paths)
            .unwrap();

        let left_vibration_action = action_set
            .create_action(
                "left_hand_vibration",
                "Left hand vibration",
                left_subaction_paths,
            )
            .unwrap();
        let right_vibration_action = action_set
            .create_action(
                "right_hand_vibration",
                "Right hand vibration",
                right_subaction_paths,
            )
            .unwrap();

        // Create action bindings:

        for (id, action) in &button_actions {
            let standard_path = BUTTON_INFO.get(id).unwrap().path;
            let hvr_path;
            let path = if platform == Platform::Hvr {
                let Some(path) = hvr_binding_path(standard_path) else {
                    continue;
                };
                hvr_path = path;
                &hvr_path
            } else {
                standard_path
            };
            match action {
                ButtonAction::Binary(action) => {
                    bindings.push(binding(action, path));
                }
                ButtonAction::Scalar(action) => {
                    bindings.push(binding(action, path));
                }
            }
        }

        let left_grip_path = if platform == Platform::Hvr {
            "/user/hand/left/input/aim/pose"
        } else {
            "/user/hand/left/input/grip/pose"
        };
        let right_grip_path = if platform == Platform::Hvr {
            "/user/hand/right/input/aim/pose"
        } else {
            "/user/hand/right/input/grip/pose"
        };
        bindings.push(binding(&left_grip_action, left_grip_path));
        bindings.push(binding(&right_grip_action, right_grip_path));

        bindings.push(binding(&left_aim_action, "/user/hand/left/input/aim/pose"));
        bindings.push(binding(
            &right_aim_action,
            "/user/hand/right/input/aim/pose",
        ));

        bindings.push(binding(
            &left_vibration_action,
            "/user/hand/left/output/haptic",
        ));
        bindings.push(binding(
            &right_vibration_action,
            "/user/hand/right/output/haptic",
        ));

        let multimodal_handle = create_ext_object("MultimodalMeta", Some(true), || {
            MultimodalMeta::new(xr_session.clone(), &extra_extensions, xr_system)
        });

        let left_detached_controller_pose_action;
        let right_detached_controller_pose_action;
        if multimodal_handle.is_some() {
            // Note: when multimodal input is enabled, both controllers and hands will always be active.
            // To be able to detect when controllers are actually held, we have to register detached
            // controllers pose; the controller pose will be diverted to the detached controllers when
            // they are not held. Currently the detached controllers pose is ignored
            left_detached_controller_pose_action = action_set
                .create_action::<xr::Posef>(
                    "left_detached_controller_pose",
                    "Left detached controller pose",
                    &[],
                )
                .unwrap();
            right_detached_controller_pose_action = action_set
                .create_action::<xr::Posef>(
                    "right_detached_controller_pose",
                    "Right detached controller pose",
                    &[],
                )
                .unwrap();

            bindings.push(binding(
                &left_detached_controller_pose_action,
                "/user/detached_controller_meta/left/input/grip/pose",
            ));
            bindings.push(binding(
                &right_detached_controller_pose_action,
                "/user/detached_controller_meta/right/input/grip/pose",
            ));
        }

        // Apply bindings:
        let bindings_result = xr_instance.suggest_interaction_profile_bindings(
            xr_instance
                .string_to_path(interaction_profile_path)
                .unwrap(),
            &bindings,
        );
        if platform == Platform::Hvr {
            if let Err(error) = bindings_result {
                // Huawei's legacy runtime rejects the whole suggestion atomically if any single
                // path is unsupported, which would leave the pose actions unbound and make
                // xrCreateActionSpace fail with ERROR_PATH_INVALID (crashing startup). Retry with
                // only the pose/haptic bindings so head/controller poses still work.
                warn!(
                    "[HVR-DIAG] controller bindings rejected: {error}; retrying with pose-only set"
                );
                let pose_only_bindings = [
                    binding(&left_grip_action, left_grip_path),
                    binding(&right_grip_action, right_grip_path),
                    binding(&left_aim_action, "/user/hand/left/input/aim/pose"),
                    binding(&right_aim_action, "/user/hand/right/input/aim/pose"),
                ];
                let pose_result = xr_instance.suggest_interaction_profile_bindings(
                    xr_instance
                        .string_to_path(interaction_profile_path)
                        .unwrap(),
                    &pose_only_bindings,
                );
                if let Err(error) = pose_result {
                    warn!("[HVR-DIAG] pose-only bindings also rejected: {error}");
                } else {
                    info!("[HVR-DIAG] pose-only bindings accepted");
                }
            }
        } else {
            bindings_result.unwrap();
        }

        // QiYu's OpenXR runtime (Monado-based) does not activate the Oculus Touch
        // profile for its physical controllers and exposes no vendor controller
        // interaction extension, so the suggestion above yields no controller input.
        // Every conformant runtime must support the Khronos simple controller, which
        // Monado maps real controllers onto, so additionally suggest those bindings to
        // recover controller pose, trigger (select) and menu input.
        if platform == Platform::Qiyu {
            let mut simple_bindings = vec![
                binding(&left_grip_action, "/user/hand/left/input/grip/pose"),
                binding(&right_grip_action, "/user/hand/right/input/grip/pose"),
                binding(&left_aim_action, "/user/hand/left/input/aim/pose"),
                binding(&right_aim_action, "/user/hand/right/input/aim/pose"),
                binding(&left_vibration_action, "/user/hand/left/output/haptic"),
                binding(&right_vibration_action, "/user/hand/right/output/haptic"),
            ];
            if let Some(ButtonAction::Scalar(action)) =
                button_actions.get(&*LEFT_TRIGGER_VALUE_ID)
            {
                simple_bindings.push(binding(action, "/user/hand/left/input/select/click"));
            }
            if let Some(ButtonAction::Scalar(action)) =
                button_actions.get(&*RIGHT_TRIGGER_VALUE_ID)
            {
                simple_bindings.push(binding(action, "/user/hand/right/input/select/click"));
            }
            if let Some(ButtonAction::Binary(action)) = button_actions.get(&*LEFT_MENU_CLICK_ID) {
                simple_bindings.push(binding(action, "/user/hand/left/input/menu/click"));
            }
            if let Some(ButtonAction::Binary(action)) = button_actions.get(&*RIGHT_SYSTEM_CLICK_ID)
            {
                simple_bindings.push(binding(action, "/user/hand/right/input/menu/click"));
            }

            let res = xr_instance.suggest_interaction_profile_bindings(
                xr_instance
                    .string_to_path("/interaction_profiles/khr/simple_controller")
                    .unwrap(),
                &simple_bindings,
            );
            match res {
                Ok(()) => info!("[QIYU] khr/simple_controller bindings accepted"),
                Err(error) => warn!("[QIYU] khr/simple_controller bindings rejected: {error}"),
            }
        }

        // Pico headsets require calling get_system_properties to test for extensions, because all
        // extensions function pointers are available even if the feature is not supported by the
        // hardware. The full checks are done in supports_eye_gaze_interaction. This is required
        // to avoid a crash when requesting the EYE_TRACKING permission.
        let combined_eyes_source = if !platform.is_quest()
            && !platform.is_vive()
            && extra_extensions::supports_eye_gaze_interaction(&xr_session, xr_system)
        {
            // todo: research Pico Neo 3 Pro Eye platform detection
            #[cfg(target_os = "android")]
            if matches!(
                platform,
                Platform::PicoNeo3 | Platform::Pico4Pro | Platform::Pico4Enterprise
            ) {
                alvr_system_info::try_get_permission("com.picovr.permission.EYE_TRACKING")
            }

            let action = action_set
                .create_action("combined_eye_gaze", "Combined eye gaze", &[])
                .unwrap();

            let res = xr_instance.suggest_interaction_profile_bindings(
                xr_instance
                    .string_to_path("/interaction_profiles/ext/eye_gaze_interaction")
                    .unwrap(),
                &[binding(&action, "/user/eyes_ext/input/gaze_ext/pose")],
            );
            if res.is_err() {
                warn!("Failed to register combined eye gaze input: {res:?}");
            }

            let space = action
                .create_space(xr_session.clone(), xr::Path::NULL, xr::Posef::IDENTITY)
                .unwrap();

            Some((action, space))
        } else {
            None
        };

        xr_session.attach_action_sets(&[&action_set]).unwrap();

        // Diagnostic: log the interaction profile the runtime actually activated for each hand.
        // On QiYu this reveals the real /interaction_profiles/... path (if any) so controller
        // bindings can be tailored precisely instead of relying on khr/simple_controller.
        for (hand_name, hand_path) in [
            ("left", "/user/hand/left"),
            ("right", "/user/hand/right"),
        ] {
            if let Ok(top_level) = xr_instance.string_to_path(hand_path) {
                match xr_session.current_interaction_profile(top_level) {
                    Ok(profile_path) => {
                        let profile_str = xr_instance
                            .path_to_string(profile_path)
                            .unwrap_or_else(|_| "<unknown>".into());
                        info!(
                            "[PROFILE-DIAG] {hand_name} hand current interaction profile: '{profile_str}'"
                        );
                    }
                    Err(error) => {
                        info!("[PROFILE-DIAG] {hand_name} hand interaction profile query failed: {error}");
                    }
                }
            }
        }

        // Huawei's legacy runtime can reject controller interaction bindings entirely, which then
        // makes xrCreateActionSpace fail with ERROR_PATH_INVALID and crashes startup (dark screen).
        // To keep the lobby rendering, fall back to a LOCAL reference space placeholder when the
        // controller action space cannot be created. This preserves head tracking / rendering; real
        // controller tracking is unavailable until Huawei accepts the bindings.
        let create_pose_space = |action: &xr::Action<xr::Posef>,
                                 subaction_path: xr::Path|
         -> xr::Space {
            match action.create_space(xr_session.clone(), subaction_path, xr::Posef::IDENTITY) {
                Ok(space) => space,
                Err(error) if platform == Platform::Hvr => {
                    warn!(
                        "[HVR-DIAG] controller action space rejected: {error}; \
                         using LOCAL reference space placeholder"
                    );
                    xr_session
                        .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
                        .unwrap()
                }
                Err(error) => panic!("{error}"),
            }
        };

        let left_subaction_path = hvr_hand_paths
            .as_ref()
            .map_or(xr::Path::NULL, |paths| paths[0]);
        let right_subaction_path = hvr_hand_paths
            .as_ref()
            .map_or(xr::Path::NULL, |paths| paths[1]);
        let left_grip_space = create_pose_space(&left_grip_action, left_subaction_path);
        let right_grip_space = create_pose_space(&right_grip_action, right_subaction_path);

        let left_aim_space = create_pose_space(&left_aim_action, left_subaction_path);
        let right_aim_space = create_pose_space(&right_aim_action, right_subaction_path);

        let left_hand_tracker = create_ext_object("HandTracker (left)", Some(true), || {
            xr_session.create_hand_tracker(xr::Hand::LEFT)
        });
        let right_hand_tracker = create_ext_object("HandTracker (right)", Some(true), || {
            xr_session.create_hand_tracker(xr::Hand::RIGHT)
        });

        // Note: HTC facial tracking can only be created at startup before xrBeginSession. We don't
        // know the reason.
        let eye_tracker_htc = create_ext_object("FacialTrackerHTC (eyes)", Some(true), || {
            FacialTrackerHTC::new(
                xr_session.clone(),
                xr_system,
                xr::FacialTrackingTypeHTC::EYE_DEFAULT,
            )
        });
        let lip_tracker_htc = create_ext_object("FacialTrackerHTC (lips)", Some(true), || {
            FacialTrackerHTC::new(
                xr_session.clone(),
                xr_system,
                xr::FacialTrackingTypeHTC::LIP_DEFAULT,
            )
        });

        Self {
            xr_session,
            xr_system,
            extra_extensions,
            platform,
            action_set,
            button_actions,
            hands_interaction: [
                HandInteraction {
                    controllers_profile_id,
                    pose_offset: get_controller_offset(platform, false),
                    subaction_path: left_subaction_path,
                    grip_action: left_grip_action,
                    grip_space: left_grip_space,
                    aim_action: left_aim_action,
                    aim_space: left_aim_space,
                    vibration_action: left_vibration_action,
                    skeleton_tracker: left_hand_tracker,
                },
                HandInteraction {
                    controllers_profile_id,
                    pose_offset: get_controller_offset(platform, true),
                    subaction_path: right_subaction_path,
                    grip_action: right_grip_action,
                    grip_space: right_grip_space,
                    aim_action: right_aim_action,
                    aim_space: right_aim_space,
                    vibration_action: right_vibration_action,
                    skeleton_tracker: right_hand_tracker,
                },
            ],
            hvr_position_tracking,
            multimodal_handle,
            multimodal_hands_enabled: false,
            face_sources: FaceSources {
                combined_eyes_source,
                eye_tracker_fb: None,
                face_tracker_fb: None,
                eye_tracker_htc,
                lip_tracker_htc,
                face_tracker_pico: None,
            },
            body_sources: BodySources {
                body_tracker_fb: None,
                body_tracker_bd: None,
                motion_tracker_bd: None,
            },
        }
    }

    pub fn select_sources(&mut self, config: &InteractionSourcesConfig) {
        // First of all, disable/delete all sources. This ensures there are no conflicts
        if let Some(handle) = &mut self.multimodal_handle {
            handle.pause().ok();
        }
        if let Some(face_tracker) = &self.face_sources.face_tracker_pico {
            face_tracker.stop_face_tracking().ok();
        }
        self.multimodal_hands_enabled = false;
        self.face_sources.eye_tracker_fb = None;
        self.face_sources.face_tracker_fb = None;
        self.face_sources.face_tracker_pico = None;
        self.body_sources.body_tracker_fb = None;
        self.body_sources.body_tracker_bd = None;
        self.body_sources.motion_tracker_bd = None;

        // todo: check which permissions are needed for htc
        if let Some(config) = &config.face_tracking {
            if (config.eye_tracking_fb) && matches!(self.platform, Platform::QuestPro) {
                #[cfg(target_os = "android")]
                alvr_system_info::try_get_permission("com.oculus.permission.EYE_TRACKING")
            }
            if config.face_tracking_fb && matches!(self.platform, Platform::QuestPro) {
                #[cfg(target_os = "android")]
                {
                    alvr_system_info::try_get_permission("android.permission.RECORD_AUDIO");
                    alvr_system_info::try_get_permission("com.oculus.permission.FACE_TRACKING")
                }
            }
            if config.face_tracking_pico && self.platform.is_pico() {
                #[cfg(target_os = "android")]
                {
                    alvr_system_info::try_get_permission("android.permission.RECORD_AUDIO");
                    alvr_system_info::try_get_permission("com.picovr.permission.FACE_TRACKING")
                }
            }
        }

        if let Some(config) = &config.body_tracking {
            if (config.body_tracking_fb.enabled())
                && self.platform.is_quest()
                && self.platform != Platform::Quest1
            {
                #[cfg(target_os = "android")]
                alvr_system_info::try_get_permission("com.oculus.permission.BODY_TRACKING")
            }
        }

        // Note: We cannot enable multimodal if fb body tracking is active. It would result in a
        // ERROR_RUNTIME_FAILURE crash.
        if config.body_tracking.is_none() && config.prefers_multimodal_input {
            if let Some(handle) = &mut self.multimodal_handle {
                if handle.resume().is_ok() {
                    self.multimodal_hands_enabled = true;
                }
            }
        }

        self.face_sources.eye_tracker_fb = create_ext_object(
            "EyeTrackerSocial",
            config.face_tracking.as_ref().map(|s| s.eye_tracking_fb),
            || EyeTrackerSocial::new(&self.xr_session),
        );

        self.face_sources.face_tracker_fb = create_ext_object(
            "FaceTracker2FB",
            config.face_tracking.as_ref().map(|s| s.face_tracking_fb),
            || FaceTracker2FB::new(self.xr_session.clone(), true, true),
        );

        self.face_sources.face_tracker_pico = create_ext_object(
            "FaceTrackerPico",
            config.face_tracking.as_ref().map(|s| s.face_tracking_pico),
            || FaceTrackerPico::new(self.xr_session.clone()),
        );

        self.body_sources.body_tracker_fb = create_ext_object(
            "BodyTrackerFB (full set)",
            config
                .body_tracking
                .as_ref()
                .and_then(|s| s.body_tracking_fb.as_option())
                .map(|c| c.full_body),
            || BodyTrackerFB::new(&self.xr_session, *BODY_JOINT_SET_FULL_BODY_META),
        )
        .map(|tracker| (tracker, FULL_BODY_JOINT_COUNT_META))
        .or_else(|| {
            create_ext_object(
                "BodyTrackerFB (default set)",
                config
                    .body_tracking
                    .as_ref()
                    .map(|s| s.body_tracking_fb.enabled()),
                || BodyTrackerFB::new(&self.xr_session, xr::BodyJointSetFB::DEFAULT),
            )
            .map(|tracker| (tracker, xr::BodyJointFB::COUNT.into_raw() as usize))
        });

        if let Some(body_tracking_config) = config
            .body_tracking
            .as_ref()
            .and_then(|s| s.body_tracking_bd.as_option())
        {
            match body_tracking_config {
                BodyTrackingBDConfig::BodyTracking {
                    high_accuracy,
                    prompt_calibration_on_start,
                } => {
                    self.body_sources.body_tracker_bd = create_ext_object(
                        "BodyTrackerBD (high accuracy)",
                        Some(*high_accuracy),
                        || {
                            BodyTrackerBD::new(
                                self.xr_session.clone(),
                                BodyJointSetBD::FULL_BODY_JOINTS,
                                &self.extra_extensions,
                                self.xr_system,
                                *prompt_calibration_on_start,
                            )
                        },
                    )
                    .or_else(|| {
                        create_ext_object("BodyTrackerBD (low accuracy)", Some(true), || {
                            BodyTrackerBD::new(
                                self.xr_session.clone(),
                                BodyJointSetBD::BODY_WITHOUT_ARM,
                                &self.extra_extensions,
                                self.xr_system,
                                *prompt_calibration_on_start,
                            )
                        })
                    })
                }
                BodyTrackingBDConfig::ObjectTracking => {
                    self.body_sources.motion_tracker_bd =
                        create_ext_object("MotionTrackerBD (object tracking)", Some(true), || {
                            MotionTrackerBD::new(self.xr_session.clone(), &self.extra_extensions)
                        });
                }
            }
        }

        if let Some(face_tracker) = &self.face_sources.face_tracker_pico {
            face_tracker.start_face_tracking().ok();
        }
    }
}

pub fn get_reference_space(
    xr_session: &xr::Session<xr::OpenGlEs>,
    ty: xr::ReferenceSpaceType,
) -> xr::Space {
    xr_session
        .create_reference_space(ty, xr::Posef::IDENTITY)
        .unwrap()
}

// Huawei VR only exposes a LOCAL reference space, whose origin sits at the headset's initial
// (eye-level) pose rather than the floor. ALVR's lobby and world assume a floor-level origin
// (STAGE / LOCAL_FLOOR semantics), so without compensation the ground is drawn at eye height.
// Push the origin down by an average standing eye height so the floor lands under the user's feet.
const HVR_EYE_HEIGHT_METERS: f32 = 1.5;

pub fn get_hvr_floor_reference_space(xr_session: &xr::Session<xr::OpenGlEs>) -> xr::Space {
    let floor_pose = xr::Posef {
        orientation: xr::Quaternionf::IDENTITY,
        position: xr::Vector3f {
            x: 0.0,
            y: -HVR_EYE_HEIGHT_METERS,
            z: 0.0,
        },
    };

    xr_session
        .create_reference_space(xr::ReferenceSpaceType::LOCAL, floor_pose)
        .unwrap()
}

pub fn get_head_data(
    xr_session: &xr::Session<xr::OpenGlEs>,
    platform: Platform,
    stage_reference_space: &xr::Space,
    view_reference_space: &xr::Space,
    time: Duration,
    future_time: Duration,
    last_view_params: &[ViewParams; 2],
) -> Option<(DeviceMotion, Option<[ViewParams; 2]>)> {
    let xr_time = crate::to_xr_time(time);

    let (head_location, head_velocity) = view_reference_space
        .relate(stage_reference_space, xr_time)
        .ok()?;

    if !head_location
        .location_flags
        .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
    {
        return None;
    }

    let (view_flags, views) = xr_session
        .locate_views(
            xr::ViewConfigurationType::PRIMARY_STEREO,
            xr_time,
            stage_reference_space,
        )
        .ok()?;

    if !view_flags.contains(xr::ViewStateFlags::POSITION_VALID)
        || !view_flags.contains(xr::ViewStateFlags::ORIENTATION_VALID)
    {
        return None;
    }

    let mut motion = DeviceMotion {
        pose: crate::from_xr_pose(head_location.pose),
        linear_velocity: if head_velocity
            .velocity_flags
            .contains(xr::SpaceVelocityFlags::LINEAR_VALID)
        {
            crate::from_xr_vec3(head_velocity.linear_velocity)
        } else {
            Vec3::ZERO
        },
        angular_velocity: if head_velocity
            .velocity_flags
            .contains(xr::SpaceVelocityFlags::ANGULAR_VALID)
        {
            crate::from_xr_vec3(head_velocity.angular_velocity)
        } else {
            Vec3::ZERO
        },
    };

    // Some headsets use wrong frame of reference for linear and angular velocities.
    if platform.is_pico() || platform.is_vive() || platform.is_yvr() {
        let xr_future_time = crate::to_xr_time(future_time);

        let predicted_location = view_reference_space
            .locate(stage_reference_space, xr_future_time)
            .ok()?;

        if !predicted_location
            .location_flags
            .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
        {
            return None;
        }

        let time_offset = future_time.saturating_sub(time);

        if !time_offset.is_zero() {
            let time_offset_s = time_offset.as_secs_f32();

            motion.linear_velocity = (crate::from_xr_vec3(predicted_location.pose.position)
                - motion.pose.position)
                / time_offset_s;
            motion.angular_velocity = (crate::from_xr_quat(predicted_location.pose.orientation)
                * motion.pose.orientation.inverse())
            .to_scaled_axis()
                / time_offset_s;
        }
    }

    let last_ipd_m = last_view_params[0]
        .pose
        .position
        .distance(last_view_params[1].pose.position);
    let current_ipd_m = crate::from_xr_vec3(views[1].pose.position)
        .distance(crate::from_xr_vec3(views[0].pose.position));
    let view_params = if f32::abs(current_ipd_m - last_ipd_m) > IPD_CHANGE_EPS {
        Some([
            ViewParams {
                pose: motion.pose.inverse() * crate::from_xr_pose(views[0].pose),
                fov: crate::from_xr_fov(views[0].fov),
            },
            ViewParams {
                pose: motion.pose.inverse() * crate::from_xr_pose(views[1].pose),
                fov: crate::from_xr_fov(views[1].fov),
            },
        ])
    } else {
        None
    };

    Some((motion, view_params))
}

#[expect(clippy::too_many_arguments)]
pub fn get_hand_data(
    xr_session: &xr::Session<xr::OpenGlEs>,
    platform: Platform,
    reference_space: &xr::Space,
    time: Duration,
    future_time: Duration,
    hand_source: &HandInteraction,
    last_controller_pose: &mut Pose,
    last_palm_pose: &mut Pose,
) -> (Option<DeviceMotion>, Option<[Pose; 26]>) {
    let xr_time = crate::to_xr_time(time);

    // Huawei's legacy runtime can report a pose action inactive even while xrLocateSpace returns a
    // fully tracked controller pose. Its official sample locates the action spaces independently
    // of isActive, so accept valid space locations on HVR and let the location flags decide.
    let pose_active = platform == Platform::Hvr
        || hand_source
            .grip_action
            .is_active(xr_session, hand_source.subaction_path)
            .unwrap_or(false);
    let controller_motion = if pose_active {
        if let Ok((location, velocity)) = hand_source.grip_space.relate(reference_space, xr_time) {
            let orientation_valid = location
                .location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_VALID);
            let position_valid = location
                .location_flags
                .contains(xr::SpaceLocationFlags::POSITION_VALID);

            if orientation_valid {
                last_controller_pose.orientation = crate::from_xr_quat(location.pose.orientation);
            }

            if position_valid {
                last_controller_pose.position = crate::from_xr_vec3(location.pose.position);
            }

            let pose = *last_controller_pose * hand_source.pose_offset;

            let mut linear_velocity = crate::from_xr_vec3(velocity.linear_velocity);
            let mut angular_velocity = crate::from_xr_vec3(velocity.angular_velocity);

            // Some headsets use wrong frame of reference for linear and angular velocities.
            if platform.is_pico() || platform.is_vive() {
                let xr_future_time = crate::to_xr_time(future_time);

                let maybe_future_location = hand_source
                    .grip_space
                    .locate(reference_space, xr_future_time);

                if let Ok(future_location) = maybe_future_location {
                    if future_location.location_flags.contains(
                        xr::SpaceLocationFlags::ORIENTATION_VALID
                            | xr::SpaceLocationFlags::POSITION_VALID,
                    ) {
                        let time_offset = future_time.saturating_sub(time);

                        if !time_offset.is_zero() {
                            let time_offset_s = time_offset.as_secs_f32();

                            linear_velocity = (crate::from_xr_vec3(future_location.pose.position)
                                - last_controller_pose.position)
                                / time_offset_s;
                            angular_velocity =
                                (crate::from_xr_quat(future_location.pose.orientation)
                                    * last_controller_pose.orientation.inverse())
                                .to_scaled_axis()
                                    / time_offset_s;
                        }
                    }
                }
            }

            Some(DeviceMotion {
                pose,
                linear_velocity,
                angular_velocity,
            })
        } else {
            None
        }
    } else {
        None
    };

    let hand_joints = if let Some(tracker) = &hand_source.skeleton_tracker {
        let xr_now = crate::xr_runtime_now(xr_session.instance()).unwrap_or(xr_time);

        if let Some(joint_locations) = reference_space
            .locate_hand_joints(tracker, xr_now)
            .ok()
            .flatten()
        {
            if joint_locations[0]
                .location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
            {
                last_palm_pose.orientation =
                    crate::from_xr_quat(joint_locations[0].pose.orientation);
            }

            if joint_locations[0]
                .location_flags
                .contains(xr::SpaceLocationFlags::POSITION_VALID)
            {
                last_palm_pose.position = crate::from_xr_vec3(joint_locations[0].pose.position);
            }

            let mut joints: [_; 26] = joint_locations
                .iter()
                .map(|j| crate::from_xr_pose(j.pose))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();

            joints[0] = *last_palm_pose;

            Some(joints)
        } else {
            None
        }
    } else {
        None
    };

    (controller_motion, hand_joints)
}

fn adjust_hvr_thumbsticks(values: &mut HashMap<u64, (ButtonValue, bool)>, position_tracking: bool) {
    const HVR_THUMBSTICK_DEADZONE: f32 = 0.16;

    for (x_id, y_id) in [
        (*LEFT_THUMBSTICK_X_ID, *LEFT_THUMBSTICK_Y_ID),
        (*RIGHT_THUMBSTICK_X_ID, *RIGHT_THUMBSTICK_Y_ID),
    ] {
        let Some((ButtonValue::Scalar(x), _)) = values.get(&x_id).copied() else {
            continue;
        };
        let Some((ButtonValue::Scalar(y), _)) = values.get(&y_id).copied() else {
            continue;
        };

        let (x, y) = if x.abs() < HVR_THUMBSTICK_DEADZONE && y.abs() < HVR_THUMBSTICK_DEADZONE {
            (0.0, 0.0)
        } else if position_tracking {
            (x, -y)
        } else {
            (x, y)
        };

        values.get_mut(&x_id).unwrap().0 = ButtonValue::Scalar(x);
        values.get_mut(&y_id).unwrap().0 = ButtonValue::Scalar(y);
    }
}

fn button_value_changed(previous: Option<&ButtonValue>, current: ButtonValue) -> bool {
    match (previous, current) {
        (Some(ButtonValue::Binary(previous)), ButtonValue::Binary(current)) => *previous != current,
        (Some(ButtonValue::Scalar(previous)), ButtonValue::Scalar(current)) => *previous != current,
        _ => true,
    }
}

pub fn update_buttons(
    xr_session: &xr::Session<xr::OpenGlEs>,
    button_actions: &HashMap<u64, ButtonAction>,
    platform: Platform,
    hvr_position_tracking: bool,
    hvr_last_values: &mut HashMap<u64, ButtonValue>,
) -> Vec<ButtonEntry> {
    let mut values = HashMap::with_capacity(button_actions.len());
    for (id, action) in button_actions {
        let subaction_path = if platform == Platform::Hvr {
            let path = BUTTON_INFO.get(id).unwrap().path;
            xr_session
                .instance()
                .string_to_path(if path.starts_with("/user/hand/left/") {
                    "/user/hand/left"
                } else {
                    "/user/hand/right"
                })
                .unwrap()
        } else {
            xr::Path::NULL
        };
        match action {
            ButtonAction::Binary(action) => {
                let Ok(state) = action.state(xr_session, subaction_path) else {
                    continue;
                };
                values.insert(
                    *id,
                    (
                        ButtonValue::Binary(state.current_state),
                        state.changed_since_last_sync,
                    ),
                );
            }
            ButtonAction::Scalar(action) => {
                let Ok(state) = action.state(xr_session, subaction_path) else {
                    continue;
                };
                values.insert(
                    *id,
                    (
                        ButtonValue::Scalar(state.current_state),
                        state.changed_since_last_sync,
                    ),
                );
            }
        }
    }

    if platform == Platform::Hvr {
        adjust_hvr_thumbsticks(&mut values, hvr_position_tracking);
    }

    let mut button_entries = Vec::with_capacity(2);
    for (id, (value, changed_since_last_sync)) in values {
        let changed = if platform == Platform::Hvr {
            button_value_changed(hvr_last_values.get(&id), value)
        } else {
            changed_since_last_sync
        };
        if changed {
            button_entries.push(ButtonEntry { path_id: id, value });
        }
        if platform == Platform::Hvr {
            hvr_last_values.insert(id, value);
        }
    }

    button_entries
}

pub fn get_eye_gazes(
    xr_session: &xr::Session<xr::OpenGlEs>,
    sources: &FaceSources,
    reference_space: &xr::Space,
    time: Duration,
) -> [Option<Pose>; 2] {
    let xr_time = crate::to_xr_time(time);

    'fb_eyes: {
        let Some(tracker) = &sources.eye_tracker_fb else {
            break 'fb_eyes;
        };

        if let Ok(gazes) = tracker.get_eye_gazes(reference_space, xr_time) {
            return [
                gazes[0].map(crate::from_xr_pose),
                gazes[1].map(crate::from_xr_pose),
            ];
        }
    };

    let Some((eyes_action, eyes_space)) = &sources.combined_eyes_source else {
        return [None, None];
    };
    if !eyes_action
        .is_active(xr_session, xr::Path::NULL)
        .unwrap_or(false)
    {
        return [None, None];
    }

    if let Ok(location) = eyes_space.locate(reference_space, xr_time) {
        [
            location
                .location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_TRACKED)
                .then(|| crate::from_xr_pose(location.pose)),
            None,
        ]
    } else {
        [None, None]
    }
}

pub fn get_fb_face_expression(context: &FaceSources, time: Duration) -> Option<Vec<f32>> {
    let xr_time = crate::to_xr_time(time);

    context
        .face_tracker_fb
        .as_ref()
        .and_then(|t| t.get_face_expression_weights(xr_time).ok().flatten())
        .map(|weights| weights.into_iter().collect())
}

pub fn get_pico_face_expression(context: &FaceSources, time: Duration) -> Option<Vec<f32>> {
    let xr_time = crate::to_xr_time(time);

    context
        .face_tracker_pico
        .as_ref()
        .and_then(|t| t.get_face_tracking_data(xr_time).ok().flatten())
        .map(|weights| weights.into_iter().collect())
}

pub fn get_htc_eye_expression(context: &FaceSources, time: Duration) -> Option<Vec<f32>> {
    let xr_time = crate::to_xr_time(time);

    context
        .eye_tracker_htc
        .as_ref()
        .and_then(|t| t.get_facial_expressions(xr_time).ok().flatten())
}

pub fn get_htc_lip_expression(context: &FaceSources, time: Duration) -> Option<Vec<f32>> {
    let xr_time = crate::to_xr_time(time);

    context
        .lip_tracker_htc
        .as_ref()
        .and_then(|t| t.get_facial_expressions(xr_time).ok().flatten())
}

pub fn get_fb_body_skeleton(
    reference_space: &xr::Space,
    time: xr::Time,
    body_tracker: &BodyTrackerFB,
    joint_count: usize,
) -> Option<Vec<Option<Pose>>> {
    body_tracker
        .locate_body_joints(time, reference_space, joint_count)
        .ok()
        .flatten()
        .map(|joints| {
            let valid_flags: SpaceLocationFlags =
                SpaceLocationFlags::ORIENTATION_VALID | SpaceLocationFlags::POSITION_VALID;

            joints
                .iter()
                .map(|joint| {
                    joint
                        .location_flags
                        .contains(valid_flags)
                        .then(|| crate::from_xr_pose(joint.pose))
                })
                .collect()
        })
}

pub fn get_fb_body_tracking_points(
    reference_space: &xr::Space,
    time: Duration,
    body_tracker: &BodyTrackerFB,
    joint_count: usize,
) -> Vec<(u64, DeviceMotion)> {
    let xr_time = crate::to_xr_time(time);

    if let Some(joint_locations) = body_tracker
        .locate_body_joints(xr_time, reference_space, joint_count)
        .ok()
        .flatten()
    {
        let valid_flags: SpaceLocationFlags =
            SpaceLocationFlags::ORIENTATION_VALID | SpaceLocationFlags::POSITION_VALID;

        let mut joints = Vec::<(u64, DeviceMotion)>::with_capacity(8);

        if let Some(joint) = joint_locations.get(xr::BodyJointFB::CHEST.into_raw() as usize) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_CHEST_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(xr::BodyJointFB::HIPS.into_raw() as usize) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_HIPS_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) =
            joint_locations.get(xr::BodyJointFB::LEFT_ARM_LOWER.into_raw() as usize)
        {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_ELBOW_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) =
            joint_locations.get(xr::BodyJointFB::RIGHT_ARM_LOWER.into_raw() as usize)
        {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_ELBOW_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_LEFT_LOWER_LEG_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_KNEE_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_LEFT_FOOT_BALL_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_FOOT_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_RIGHT_LOWER_LEG_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_KNEE_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_RIGHT_FOOT_BALL_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_FOOT_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        return joints;
    }

    Vec::new()
}

pub fn get_bd_motion_trackers(
    time: Duration,
    motion_tracker: &MotionTrackerBD,
) -> Vec<(u64, DeviceMotion)> {
    let xr_time = crate::to_xr_time(time);

    if let Some(mut trackers) = motion_tracker
        .locate_motion_trackers(xr_time)
        .ok()
        .flatten()
    {
        let mut joints = Vec::<(u64, DeviceMotion)>::with_capacity(3);

        let joints_ids = [*BODY_HIPS_ID, *BODY_LEFT_FOOT_ID, *BODY_RIGHT_FOOT_ID];

        trackers.sort_by(|a, b| a.serial.cmp(&b.serial));

        for (i, item) in trackers.iter().enumerate() {
            joints.push((
                joints_ids[i],
                DeviceMotion {
                    pose: crate::from_xr_pose(item.local_pose.pose),
                    linear_velocity: crate::from_xr_vec3(item.local_pose.linear_velocity),
                    angular_velocity: crate::from_xr_vec3(item.local_pose.angular_velocity),
                },
            ))
        }

        return joints;
    }

    Vec::new()
}

pub fn get_bd_body_skeleton(
    reference_space: &xr::Space,
    time: xr::Time,
    body_tracker: &BodyTrackerBD,
) -> Option<Vec<Option<Pose>>> {
    body_tracker
        .locate_body_joints(time, reference_space)
        .ok()
        .flatten()
        .map(|joints| {
            let valid_flags: SpaceLocationFlags =
                SpaceLocationFlags::ORIENTATION_VALID | SpaceLocationFlags::POSITION_VALID;

            joints
                .iter()
                .map(|joint| {
                    joint
                        .location_flags
                        .contains(valid_flags)
                        .then(|| crate::from_xr_pose(joint.pose))
                })
                .collect()
        })
}

pub fn get_bd_body_tracking_points(
    reference_space: &xr::Space,
    time: Duration,
    body_tracker: &BodyTrackerBD,
) -> Vec<(u64, DeviceMotion)> {
    let xr_time = crate::to_xr_time(time);

    if let Some(joint_locations) = body_tracker
        .locate_body_joints(xr_time, reference_space)
        .ok()
        .flatten()
    {
        let valid_flags: SpaceLocationFlags =
            SpaceLocationFlags::ORIENTATION_VALID | SpaceLocationFlags::POSITION_VALID;

        let mut joints = Vec::<(u64, DeviceMotion)>::with_capacity(8);

        if let Some(joint) = joint_locations.get(BODY_JOINT_SPINE3_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_CHEST_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_PELVIS_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_HIPS_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_LEFT_ELBOW_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_ELBOW_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_RIGHT_ELBOW_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_ELBOW_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_LEFT_KNEE_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_KNEE_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_LEFT_FOOT_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_FOOT_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_RIGHT_KNEE_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_KNEE_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(BODY_JOINT_RIGHT_FOOT_BD) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_FOOT_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        return joints;
    }

    Vec::new()
}
