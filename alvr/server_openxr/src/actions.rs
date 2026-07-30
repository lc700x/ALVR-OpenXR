//! OpenXR action system.
//!
//! Minimal implementation: action sets/actions get opaque handles, paths are
//! interned, and the current interaction profile is hardcoded to Oculus Touch
//! (blueprint A9a). Action *state* is reported inactive for now — real input
//! mapping from the ALVR client lands with controller wire-up. This is enough
//! for a conformance app (`hello_xr`) to create/attach/sync actions without
//! erroring.

#![allow(non_snake_case)]

use alvr_common::{
    once_cell::sync::Lazy, parking_lot::Mutex, BUTTON_INFO, LEFT_SQUEEZE_CLICK_ID,
    LEFT_SQUEEZE_VALUE_ID, LEFT_THUMBSTICK_X_ID, LEFT_THUMBSTICK_Y_ID,
};
use alvr_packets::{ButtonEntry, ButtonValue};
use openxr_sys as sys;
// openxr_sys::Handle is not pub-exported at crate root in 0.10; methods are inherent
use std::{
    collections::HashMap,
    ffi::{c_char, CStr},
};

const OCULUS_TOUCH_PROFILE: &str = "/interaction_profiles/oculus/touch_controller";
const BOOLEAN_ACTION_THRESHOLD: f32 = 0.5;

/// Interned path strings; `Path` value is index+1 (0 reserved for null).
static PATHS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Monotonic counters for action-set / action / action-space handles.
static NEXT_ACTION_SET: Mutex<u64> = Mutex::new(1);
static NEXT_ACTION: Mutex<u64> = Mutex::new(1);
static NEXT_ACTION_SPACE: Mutex<u64> = Mutex::new(0x1000);
/// The hand declared when each action is created. A pose action normally has
/// one subaction path, while generic two-hand actions intentionally map to
/// `None`.
static ACTION_HAND: Lazy<Mutex<HashMap<u64, Option<usize>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// Every action space is recorded, including ones without an unambiguous hand.
/// This lets `xrLocateSpace` report an invalid controller pose instead of
/// accidentally treating the action-space handle as a head/reference space.
static ACTION_SPACE_HAND: Lazy<Mutex<HashMap<u64, Option<usize>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// Whether each action space is an aim (pointer ray) or grip (held) pose,
/// classified from the action's suggested binding path. `xrLocateSpace` applies
/// VDXR's calibrated grip→aim transform only for aim spaces.
static ACTION_SPACE_KIND: Lazy<Mutex<HashMap<u64, PoseKind>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static ACTION_SPACE_OFFSET: Lazy<Mutex<HashMap<u64, sys::Posef>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static ACTION_BINDINGS: Lazy<Mutex<HashMap<u64, Vec<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static BUTTON_STATES: Lazy<Mutex<HashMap<String, ButtonValue>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Which OpenXR standard pose an action space represents. The client streams a
/// platform-corrected canonical grip; aim is derived by the runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PoseKind {
    Grip,
    Aim,
}

/// Classify a pose action space from the suggested binding paths of its action.
/// Paths ending in `/aim/pose` are pointer rays; everything else (including
/// `/grip/pose`) is treated as the held grip pose.
fn pose_kind_for_action(action: sys::Action) -> PoseKind {
    let bindings = ACTION_BINDINGS.lock();
    let is_aim = bindings
        .get(&action.into_raw())
        .is_some_and(|paths| paths.iter().any(|p| p.ends_with("/aim/pose")));
    if is_aim {
        PoseKind::Aim
    } else {
        PoseKind::Grip
    }
}

fn intern_path(s: &str) -> u64 {
    let mut paths = PATHS.lock();
    if let Some(i) = paths.iter().position(|p| p == s) {
        return i as u64 + 1;
    }
    paths.push(s.to_owned());
    paths.len() as u64
}

fn path_string(value: u64) -> Option<String> {
    PATHS.lock().get((value.checked_sub(1)?) as usize).cloned()
}

fn hand_for_path(path: sys::Path) -> Option<usize> {
    match path_string(path.into_raw()).as_deref() {
        Some("/user/hand/left") => Some(0),
        Some("/user/hand/right") => Some(1),
        _ => None,
    }
}

unsafe fn hand_for_action_create_info(info: &sys::ActionCreateInfo) -> Option<usize> {
    if info.count_subaction_paths != 1 || info.subaction_paths.is_null() {
        return None;
    }

    // # Safety: `subaction_paths` is non-null and contains exactly one path,
    // as checked above.
    let path = unsafe { *info.subaction_paths };
    hand_for_path(path)
}

fn resolve_action_hand(action: sys::Action, subaction_path: sys::Path) -> Option<usize> {
    hand_for_path(subaction_path).or_else(|| {
        ACTION_HAND
            .lock()
            .get(&action.into_raw())
            .copied()
            .flatten()
    })
}

/// Accept the mapped controller values emitted by `server_core`. Their paths
/// use the same OpenXR user/input path convention as interaction bindings.
pub fn update_buttons(entries: Vec<ButtonEntry>) {
    let mut states = BUTTON_STATES.lock();
    for entry in entries {
        if let Some(info) = BUTTON_INFO.get(&entry.path_id) {
            states.insert(info.path.to_owned(), entry.value);
        }
    }
}

/// Clear edge-driven client input at a session/connection boundary. VDXR reads
/// a fresh OVR input snapshot on every `xrSyncActions`; our client sends only
/// changes, so retaining this map across a reconnect would make stale trigger
/// or A/B presses appear in the next session.
pub fn clear_buttons() {
    BUTTON_STATES.lock().clear();
}

fn input_path_hand(path: &str) -> Option<usize> {
    if path.starts_with("/user/hand/left/") {
        Some(0)
    } else if path.starts_with("/user/hand/right/") {
        Some(1)
    } else {
        None
    }
}

fn action_value(get_info: &sys::ActionStateGetInfo) -> Option<ButtonValue> {
    let bindings = ACTION_BINDINGS.lock();
    let paths = bindings.get(&get_info.action.into_raw())?;
    let hand = path_string(get_info.subaction_path.into_raw());
    let controller_tracked = *crate::state::CONTROLLER_TRACKED.read();
    let states = BUTTON_STATES.lock();

    paths.iter().find_map(|path| {
        if hand.as_ref().is_some_and(|hand| !path.starts_with(hand)) {
            return None;
        }
        // Match VDXR: sources from an inactive controller are not active and
        // must report false, regardless of the last cached button value.
        if input_path_hand(path).is_some_and(|hand| !controller_tracked[hand]) {
            return None;
        }
        let value = states.get(path).copied()?;

        // Stuck-grip guard: the client sends button state edge-triggered (only on
        // change). If a `squeeze/click` release edge is ever dropped, the click
        // stays latched `true` and the grip appears held forever. The analog
        // `squeeze/value` for the same hand is a better ground truth, so if it is
        // present and below the boolean threshold, treat the click as released.
        if let ButtonValue::Binary(true) = value {
            if path.ends_with("squeeze/click") {
                let value_path = path.replace("squeeze/click", "squeeze/value");
                if let Some(squeeze) = scalar_state(&states, &value_path) {
                    if squeeze < BOOLEAN_ACTION_THRESHOLD {
                        return Some(ButtonValue::Binary(false));
                    }
                }
            }
        }

        Some(value)
    })
}

fn boolean_action_value(value: Option<ButtonValue>) -> bool {
    match value {
        Some(ButtonValue::Binary(value)) => value,
        Some(ButtonValue::Scalar(value)) => value >= BOOLEAN_ACTION_THRESHOLD,
        None => false,
    }
}

fn scalar_state(states: &HashMap<String, ButtonValue>, path: &str) -> Option<f32> {
    match states.get(path) {
        Some(ButtonValue::Scalar(value)) => Some(*value),
        Some(ButtonValue::Binary(value)) => Some(if *value { 1.0 } else { 0.0 }),
        None => None,
    }
}

/// Read a 2D axis (thumbstick / trackpad) for an action. Apps bind the 2D
/// action to the parent input path (e.g. `.../input/thumbstick`), while the
/// client reports the `.../x` and `.../y` component scalars, so combine them.
fn action_axis(get_info: &sys::ActionStateGetInfo) -> Option<(f32, f32)> {
    let bindings = ACTION_BINDINGS.lock();
    let paths = bindings.get(&get_info.action.into_raw())?;
    let hand = path_string(get_info.subaction_path.into_raw());
    let controller_tracked = *crate::state::CONTROLLER_TRACKED.read();
    let states = BUTTON_STATES.lock();

    paths.iter().find_map(|path| {
        if hand.as_ref().is_some_and(|hand| !path.starts_with(hand)) {
            return None;
        }
        if input_path_hand(path).is_some_and(|hand| !controller_tracked[hand]) {
            return None;
        }
        let x = scalar_state(&states, &format!("{path}/x"));
        let y = scalar_state(&states, &format!("{path}/y"));
        if x.is_none() && y.is_none() {
            return None;
        }
        Some((x.unwrap_or(0.0), y.unwrap_or(0.0)))
    })
}

pub unsafe extern "system" fn string_to_path(
    _instance: sys::Instance,
    path_string: *const c_char,
    path: *mut sys::Path,
) -> sys::Result {
    if path_string.is_null() || path.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let s = unsafe { CStr::from_ptr(path_string) }
        .to_string_lossy()
        .into_owned();
    unsafe { *path = sys::Path::from_raw(intern_path(&s)) };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn path_to_string(
    _instance: sys::Instance,
    path: sys::Path,
    buffer_capacity_input: u32,
    buffer_count_output: *mut u32,
    buffer: *mut c_char,
) -> sys::Result {
    let Some(s) = path_string(path.into_raw()) else {
        return sys::Result::ERROR_PATH_INVALID;
    };
    let needed = s.len() + 1;
    if !buffer_count_output.is_null() {
        unsafe { *buffer_count_output = needed as u32 };
    }
    if buffer_capacity_input == 0 {
        return sys::Result::SUCCESS;
    }
    if (buffer_capacity_input as usize) < needed || buffer.is_null() {
        return sys::Result::ERROR_SIZE_INSUFFICIENT;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr().cast(), buffer, s.len());
        *buffer.add(s.len()) = 0;
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn create_action_set(
    _instance: sys::Instance,
    _create_info: *const sys::ActionSetCreateInfo,
    action_set: *mut sys::ActionSet,
) -> sys::Result {
    if action_set.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let mut next = NEXT_ACTION_SET.lock();
    unsafe { *action_set = sys::ActionSet::from_raw(*next) };
    *next += 1;
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn destroy_action_set(_action_set: sys::ActionSet) -> sys::Result {
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn create_action(
    _action_set: sys::ActionSet,
    create_info: *const sys::ActionCreateInfo,
    action: *mut sys::Action,
) -> sys::Result {
    if action.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let mut next = NEXT_ACTION.lock();
    let raw = *next;
    unsafe { *action = sys::Action::from_raw(raw) };
    *next += 1;

    let hand = if create_info.is_null() {
        None
    } else {
        // # Safety: the OpenXR loader supplies a valid create-info pointer.
        unsafe { hand_for_action_create_info(&*create_info) }
    };
    ACTION_HAND.lock().insert(raw, hand);

    sys::Result::SUCCESS
}

pub unsafe extern "system" fn destroy_action(action: sys::Action) -> sys::Result {
    ACTION_HAND.lock().remove(&action.into_raw());
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn suggest_interaction_profile_bindings(
    _instance: sys::Instance,
    suggested_bindings: *const sys::InteractionProfileSuggestedBinding,
) -> sys::Result {
    if suggested_bindings.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let suggested = unsafe { &*suggested_bindings };
    if suggested.count_suggested_bindings > 0 && suggested.suggested_bindings.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let mut bindings = ACTION_BINDINGS.lock();
    for index in 0..suggested.count_suggested_bindings {
        let binding = unsafe { *suggested.suggested_bindings.add(index as usize) };
        let Some(path) = path_string(binding.binding.into_raw()) else {
            continue;
        };
        bindings
            .entry(binding.action.into_raw())
            .or_default()
            .push(path);
    }
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn attach_session_action_sets(
    _session: sys::Session,
    _attach_info: *const sys::SessionActionSetsAttachInfo,
) -> sys::Result {
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_current_interaction_profile(
    _session: sys::Session,
    _top_level_user_path: sys::Path,
    interaction_profile: *mut sys::InteractionProfileState,
) -> sys::Result {
    if interaction_profile.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let profile = unsafe { &mut *interaction_profile };
    profile.ty = sys::StructureType::INTERACTION_PROFILE_STATE;
    profile.interaction_profile = sys::Path::from_raw(intern_path(OCULUS_TOUCH_PROFILE));
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn create_action_space(
    _session: sys::Session,
    create_info: *const sys::ActionSpaceCreateInfo,
    space: *mut sys::Space,
) -> sys::Result {
    if create_info.is_null() || space.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }

    let info = unsafe { &*create_info };
    let mut next = NEXT_ACTION_SPACE.lock();
    let raw = *next;
    unsafe { *space = sys::Space::from_raw(raw) };
    *next += 1;

    // Apps commonly create a separate pose action per hand and leave
    // `subaction_path` null here. Resolve that canonical form through the
    // action's creation path, while still honoring an explicitly supplied path.
    let hand = resolve_action_hand(info.action, info.subaction_path);
    ACTION_SPACE_HAND.lock().insert(raw, hand);
    ACTION_SPACE_KIND
        .lock()
        .insert(raw, pose_kind_for_action(info.action));
    ACTION_SPACE_OFFSET
        .lock()
        .insert(raw, info.pose_in_action_space);

    sys::Result::SUCCESS
}

pub unsafe extern "system" fn sync_actions(
    _session: sys::Session,
    _sync_info: *const sys::ActionsSyncInfo,
) -> sys::Result {
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_action_state_boolean(
    _session: sys::Session,
    get_info: *const sys::ActionStateGetInfo,
    state: *mut sys::ActionStateBoolean,
) -> sys::Result {
    if get_info.is_null() || state.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let value = action_value(unsafe { &*get_info });
    let s = unsafe { &mut *state };
    s.ty = sys::StructureType::ACTION_STATE_BOOLEAN;
    s.current_state = if boolean_action_value(value) {
        sys::TRUE
    } else {
        sys::FALSE
    };
    s.changed_since_last_sync = sys::FALSE;
    s.last_change_time = sys::Time::from_nanos(0);
    s.is_active = if value.is_some() {
        sys::TRUE
    } else {
        sys::FALSE
    };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_action_state_float(
    _session: sys::Session,
    get_info: *const sys::ActionStateGetInfo,
    state: *mut sys::ActionStateFloat,
) -> sys::Result {
    if get_info.is_null() || state.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let value = action_value(unsafe { &*get_info });
    let s = unsafe { &mut *state };
    s.ty = sys::StructureType::ACTION_STATE_FLOAT;
    s.current_state = match value {
        Some(ButtonValue::Scalar(value)) => value,
        _ => 0.0,
    };
    s.changed_since_last_sync = sys::FALSE;
    s.last_change_time = sys::Time::from_nanos(0);
    s.is_active = if value.is_some() {
        sys::TRUE
    } else {
        sys::FALSE
    };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_action_state_vector2f(
    _session: sys::Session,
    get_info: *const sys::ActionStateGetInfo,
    state: *mut sys::ActionStateVector2f,
) -> sys::Result {
    if get_info.is_null() || state.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let axis = action_axis(unsafe { &*get_info });
    let s = unsafe { &mut *state };
    s.ty = sys::StructureType::ACTION_STATE_VECTOR2F;
    let (x, y) = axis.unwrap_or((0.0, 0.0));
    s.current_state = sys::Vector2f { x, y };
    s.changed_since_last_sync = sys::FALSE;
    s.last_change_time = sys::Time::from_nanos(0);
    s.is_active = if axis.is_some() {
        sys::TRUE
    } else {
        sys::FALSE
    };
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn get_action_state_pose(
    _session: sys::Session,
    get_info: *const sys::ActionStateGetInfo,
    state: *mut sys::ActionStatePose,
) -> sys::Result {
    if get_info.is_null() || state.is_null() {
        return sys::Result::ERROR_VALIDATION_FAILURE;
    }
    let info = unsafe { &*get_info };
    let hand = resolve_action_hand(info.action, info.subaction_path);
    let active = hand.is_some_and(|hand| crate::state::CONTROLLER_TRACKED.read()[hand]);

    let s = unsafe { &mut *state };
    s.ty = sys::StructureType::ACTION_STATE_POSE;
    s.is_active = if active { sys::TRUE } else { sys::FALSE };
    sys::Result::SUCCESS
}

pub fn action_space_hand(space: sys::Space) -> Option<usize> {
    ACTION_SPACE_HAND
        .lock()
        .get(&space.into_raw())
        .copied()
        .flatten()
}

/// The pose kind (grip or aim) recorded for an action space, if any.
pub fn action_space_kind(space: sys::Space) -> Option<PoseKind> {
    ACTION_SPACE_KIND.lock().get(&space.into_raw()).copied()
}

pub fn action_space_offset(space: sys::Space) -> Option<sys::Posef> {
    ACTION_SPACE_OFFSET.lock().get(&space.into_raw()).copied()
}

pub fn is_action_space(space: sys::Space) -> bool {
    ACTION_SPACE_HAND.lock().contains_key(&space.into_raw())
}

pub unsafe extern "system" fn apply_haptic_feedback(
    _session: sys::Session,
    _haptic_action_info: *const sys::HapticActionInfo,
    _haptic_feedback: *const sys::HapticBaseHeader,
) -> sys::Result {
    // ponytail: haptics routed to the client lands with controller wire-up.
    sys::Result::SUCCESS
}

pub unsafe extern "system" fn stop_haptic_feedback(
    _session: sys::Session,
    _haptic_action_info: *const sys::HapticActionInfo,
) -> sys::Result {
    sys::Result::SUCCESS
}

/// Register the action-system entry points into the dispatch table.
pub fn resolve(name: &[u8]) -> Option<sys::pfn::VoidFunction> {
    macro_rules! f {
        ($func:expr) => {
            Some(unsafe { std::mem::transmute::<_, sys::pfn::VoidFunction>($func) })
        };
    }
    match name {
        b"xrStringToPath" => f!(string_to_path as sys::pfn::StringToPath),
        b"xrPathToString" => f!(path_to_string as sys::pfn::PathToString),
        b"xrCreateActionSet" => f!(create_action_set as sys::pfn::CreateActionSet),
        b"xrDestroyActionSet" => f!(destroy_action_set as sys::pfn::DestroyActionSet),
        b"xrCreateAction" => f!(create_action as sys::pfn::CreateAction),
        b"xrDestroyAction" => f!(destroy_action as sys::pfn::DestroyAction),
        b"xrSuggestInteractionProfileBindings" => {
            f!(suggest_interaction_profile_bindings as sys::pfn::SuggestInteractionProfileBindings)
        }
        b"xrAttachSessionActionSets" => {
            f!(attach_session_action_sets as sys::pfn::AttachSessionActionSets)
        }
        b"xrGetCurrentInteractionProfile" => {
            f!(get_current_interaction_profile as sys::pfn::GetCurrentInteractionProfile)
        }
        b"xrCreateActionSpace" => f!(create_action_space as sys::pfn::CreateActionSpace),
        b"xrSyncActions" => f!(sync_actions as sys::pfn::SyncActions),
        b"xrGetActionStateBoolean" => {
            f!(get_action_state_boolean as sys::pfn::GetActionStateBoolean)
        }
        b"xrGetActionStateFloat" => f!(get_action_state_float as sys::pfn::GetActionStateFloat),
        b"xrGetActionStateVector2f" => {
            f!(get_action_state_vector2f as sys::pfn::GetActionStateVector2f)
        }
        b"xrGetActionStatePose" => f!(get_action_state_pose as sys::pfn::GetActionStatePose),
        b"xrApplyHapticFeedback" => f!(apply_haptic_feedback as sys::pfn::ApplyHapticFeedback),
        b"xrStopHapticFeedback" => f!(stop_haptic_feedback as sys::pfn::StopHapticFeedback),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static INPUT_TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn path_round_trips() {
        let a = intern_path("/user/hand/left");
        let b = intern_path("/user/hand/left");
        assert_eq!(a, b);
        assert_eq!(path_string(a).as_deref(), Some("/user/hand/left"));
    }

    #[test]
    fn resolves_action_entry_points() {
        assert!(resolve(b"xrSyncActions").is_some());
        assert!(resolve(b"xrNope").is_none());
    }

    #[test]
    fn resolves_pose_action_hand_when_space_subaction_path_is_null() {
        let action_raw = 0xfeed;
        ACTION_HAND.lock().insert(action_raw, Some(0));

        assert_eq!(
            resolve_action_hand(sys::Action::from_raw(action_raw), sys::Path::NULL),
            Some(0)
        );

        ACTION_HAND.lock().remove(&action_raw);
    }

    #[test]
    fn action_space_preserves_pose_offset() {
        let action_raw = 0x0ff5e7;
        ACTION_HAND.lock().insert(action_raw, Some(0));
        let info = sys::ActionSpaceCreateInfo {
            ty: sys::StructureType::ACTION_SPACE_CREATE_INFO,
            next: std::ptr::null(),
            action: sys::Action::from_raw(action_raw),
            subaction_path: sys::Path::NULL,
            pose_in_action_space: sys::Posef {
                orientation: sys::Quaternionf {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                },
                position: sys::Vector3f {
                    x: 0.01,
                    y: 0.02,
                    z: 0.03,
                },
            },
        };
        let mut space = sys::Space::NULL;

        assert_eq!(
            unsafe { create_action_space(sys::Session::NULL, &info, &mut space) },
            sys::Result::SUCCESS
        );
        let offset = action_space_offset(space).unwrap();
        assert_eq!(offset.position.x, 0.01);
        assert_eq!(offset.position.y, 0.02);
        assert_eq!(offset.position.z, 0.03);

        ACTION_HAND.lock().remove(&action_raw);
    }

    #[test]
    fn mapped_scalar_squeeze_binding_activates_boolean_action() {
        let _guard = INPUT_TEST_LOCK.lock();
        const ACTION_RAW: u64 = 0xa11ce;
        const SQUEEZE_PATH: &str = "/user/hand/left/input/squeeze/value";

        *crate::state::CONTROLLER_TRACKED.write() = [true, false];

        let action = sys::Action::from_raw(ACTION_RAW);
        let hand_path = sys::Path::from_raw(intern_path("/user/hand/left"));
        ACTION_BINDINGS
            .lock()
            .insert(ACTION_RAW, vec![SQUEEZE_PATH.to_owned()]);
        update_buttons(vec![ButtonEntry {
            path_id: *LEFT_SQUEEZE_VALUE_ID,
            value: ButtonValue::Scalar(1.0),
        }]);

        let get_info = sys::ActionStateGetInfo {
            ty: sys::StructureType::ACTION_STATE_GET_INFO,
            next: std::ptr::null(),
            action,
            subaction_path: hand_path,
        };
        let mut state = sys::ActionStateBoolean {
            ty: sys::StructureType::ACTION_STATE_BOOLEAN,
            next: std::ptr::null_mut(),
            current_state: sys::FALSE,
            changed_since_last_sync: sys::FALSE,
            last_change_time: sys::Time::from_nanos(0),
            is_active: sys::FALSE,
        };

        let result =
            unsafe { get_action_state_boolean(sys::Session::from_raw(0), &get_info, &mut state) };

        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(state.is_active, sys::TRUE);
        assert_eq!(state.current_state, sys::TRUE);

        update_buttons(vec![ButtonEntry {
            path_id: *LEFT_SQUEEZE_VALUE_ID,
            value: ButtonValue::Scalar(0.0),
        }]);
        let result =
            unsafe { get_action_state_boolean(sys::Session::from_raw(0), &get_info, &mut state) };

        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(state.is_active, sys::TRUE);
        assert_eq!(state.current_state, sys::FALSE);

        ACTION_BINDINGS.lock().remove(&ACTION_RAW);
        BUTTON_STATES.lock().remove(SQUEEZE_PATH);
        *crate::state::CONTROLLER_TRACKED.write() = [false; 2];
    }

    #[test]
    fn stuck_squeeze_click_released_by_low_analog_value() {
        let _guard = INPUT_TEST_LOCK.lock();
        const ACTION_RAW: u64 = 0xc11c2;
        const CLICK_PATH: &str = "/user/hand/left/input/squeeze/click";
        const VALUE_PATH: &str = "/user/hand/left/input/squeeze/value";

        *crate::state::CONTROLLER_TRACKED.write() = [true, false];

        let action = sys::Action::from_raw(ACTION_RAW);
        let hand_path = sys::Path::from_raw(intern_path("/user/hand/left"));
        ACTION_BINDINGS
            .lock()
            .insert(ACTION_RAW, vec![CLICK_PATH.to_owned()]);

        // Simulate a stuck click: pressed edge arrived, release edge dropped.
        update_buttons(vec![ButtonEntry {
            path_id: *LEFT_SQUEEZE_CLICK_ID,
            value: ButtonValue::Binary(true),
        }]);
        // But the analog value shows the grip is actually released.
        update_buttons(vec![ButtonEntry {
            path_id: *LEFT_SQUEEZE_VALUE_ID,
            value: ButtonValue::Scalar(0.0),
        }]);

        let get_info = sys::ActionStateGetInfo {
            ty: sys::StructureType::ACTION_STATE_GET_INFO,
            next: std::ptr::null(),
            action,
            subaction_path: hand_path,
        };
        let mut state = sys::ActionStateBoolean {
            ty: sys::StructureType::ACTION_STATE_BOOLEAN,
            next: std::ptr::null_mut(),
            current_state: sys::FALSE,
            changed_since_last_sync: sys::FALSE,
            last_change_time: sys::Time::from_nanos(0),
            is_active: sys::FALSE,
        };
        let result =
            unsafe { get_action_state_boolean(sys::Session::from_raw(0), &get_info, &mut state) };
        assert_eq!(result, sys::Result::SUCCESS);
        // The low analog value overrides the stuck click.
        assert_eq!(state.current_state, sys::FALSE);

        // When the analog value is high, the click stands.
        update_buttons(vec![ButtonEntry {
            path_id: *LEFT_SQUEEZE_VALUE_ID,
            value: ButtonValue::Scalar(1.0),
        }]);
        let result =
            unsafe { get_action_state_boolean(sys::Session::from_raw(0), &get_info, &mut state) };
        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(state.current_state, sys::TRUE);

        ACTION_BINDINGS.lock().remove(&ACTION_RAW);
        BUTTON_STATES.lock().remove(CLICK_PATH);
        BUTTON_STATES.lock().remove(VALUE_PATH);
        *crate::state::CONTROLLER_TRACKED.write() = [false; 2];
    }

    #[test]
    fn thumbstick_axis_combines_x_and_y_components() {
        let _guard = INPUT_TEST_LOCK.lock();
        const ACTION_RAW: u64 = 0x5709;
        const STICK_PATH: &str = "/user/hand/left/input/thumbstick";

        *crate::state::CONTROLLER_TRACKED.write() = [true, false];

        let action = sys::Action::from_raw(ACTION_RAW);
        let hand_path = sys::Path::from_raw(intern_path("/user/hand/left"));
        ACTION_BINDINGS
            .lock()
            .insert(ACTION_RAW, vec![STICK_PATH.to_owned()]);
        update_buttons(vec![
            ButtonEntry {
                path_id: *LEFT_THUMBSTICK_X_ID,
                value: ButtonValue::Scalar(-0.5),
            },
            ButtonEntry {
                path_id: *LEFT_THUMBSTICK_Y_ID,
                value: ButtonValue::Scalar(0.25),
            },
        ]);

        let get_info = sys::ActionStateGetInfo {
            ty: sys::StructureType::ACTION_STATE_GET_INFO,
            next: std::ptr::null(),
            action,
            subaction_path: hand_path,
        };
        let mut state = sys::ActionStateVector2f {
            ty: sys::StructureType::ACTION_STATE_VECTOR2F,
            next: std::ptr::null_mut(),
            current_state: sys::Vector2f { x: 0.0, y: 0.0 },
            changed_since_last_sync: sys::FALSE,
            last_change_time: sys::Time::from_nanos(0),
            is_active: sys::FALSE,
        };

        let result =
            unsafe { get_action_state_vector2f(sys::Session::from_raw(0), &get_info, &mut state) };

        assert_eq!(result, sys::Result::SUCCESS);
        assert_eq!(state.is_active, sys::TRUE);
        assert_eq!(state.current_state.x, -0.5);
        assert_eq!(state.current_state.y, 0.25);

        ACTION_BINDINGS.lock().remove(&ACTION_RAW);
        BUTTON_STATES
            .lock()
            .remove("/user/hand/left/input/thumbstick/x");
        BUTTON_STATES
            .lock()
            .remove("/user/hand/left/input/thumbstick/y");
        *crate::state::CONTROLLER_TRACKED.write() = [false; 2];
    }

    #[test]
    fn inactive_controller_suppresses_and_reset_clears_stale_button() {
        let _guard = INPUT_TEST_LOCK.lock();
        const ACTION_RAW: u64 = 0xab01;
        const A_PATH: &str = "/user/hand/right/input/a/click";

        ACTION_BINDINGS
            .lock()
            .insert(ACTION_RAW, vec![A_PATH.to_owned()]);
        BUTTON_STATES
            .lock()
            .insert(A_PATH.to_owned(), ButtonValue::Binary(true));

        let get_info = sys::ActionStateGetInfo {
            ty: sys::StructureType::ACTION_STATE_GET_INFO,
            next: std::ptr::null(),
            action: sys::Action::from_raw(ACTION_RAW),
            subaction_path: sys::Path::from_raw(intern_path("/user/hand/right")),
        };

        // VDXR behavior: inactive controller never exposes its stale value.
        *crate::state::CONTROLLER_TRACKED.write() = [false; 2];
        assert!(action_value(&get_info).is_none());

        // Once active the cached value is visible, then reset removes it.
        *crate::state::CONTROLLER_TRACKED.write() = [false, true];
        assert!(matches!(
            action_value(&get_info),
            Some(ButtonValue::Binary(true))
        ));
        clear_buttons();
        assert!(action_value(&get_info).is_none());

        ACTION_BINDINGS.lock().remove(&ACTION_RAW);
        *crate::state::CONTROLLER_TRACKED.write() = [false; 2];
    }

    #[test]
    fn pose_kind_classifies_aim_and_grip_from_bindings() {
        const AIM_ACTION: u64 = 0xa1;
        const GRIP_ACTION: u64 = 0x91;

        ACTION_BINDINGS.lock().insert(
            AIM_ACTION,
            vec!["/user/hand/left/input/aim/pose".to_owned()],
        );
        ACTION_BINDINGS.lock().insert(
            GRIP_ACTION,
            vec!["/user/hand/left/input/grip/pose".to_owned()],
        );

        assert_eq!(
            pose_kind_for_action(sys::Action::from_raw(AIM_ACTION)),
            PoseKind::Aim
        );
        assert_eq!(
            pose_kind_for_action(sys::Action::from_raw(GRIP_ACTION)),
            PoseKind::Grip
        );
        // An unbound pose action defaults to grip (held), never aim.
        assert_eq!(
            pose_kind_for_action(sys::Action::from_raw(0xdead)),
            PoseKind::Grip
        );

        ACTION_BINDINGS.lock().remove(&AIM_ACTION);
        ACTION_BINDINGS.lock().remove(&GRIP_ACTION);
    }
}
