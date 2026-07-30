use android_activity::AndroidApp;
use jni::{
    objects::{JClass, JObject, JString, JValue},
    JavaVM,
};
use std::{
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering},
};

static NATIVE_WINDOW: AtomicUsize = AtomicUsize::new(0);

pub fn set_native_window(native_window: *mut c_void) {
    NATIVE_WINDOW.store(native_window as usize, Ordering::SeqCst);
}

pub fn native_window() -> Option<*mut c_void> {
    let native_window = NATIVE_WINDOW.load(Ordering::SeqCst);

    (native_window != 0).then_some(native_window as *mut c_void)
}

pub fn initialize_bridge(app: &AndroidApp) -> Result<&'static str, String> {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let class_loader = env
        .call_method(
            &activity,
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )
        .and_then(|value| value.l())
        .map_err(|error| error.to_string())?;
    let bridge_class_name = JObject::from(
        env.new_string("com.huawei.hvr.LibUpdateClient")
            .map_err(|error| error.to_string())?,
    );
    let bridge_class = env
        .call_method(
            class_loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&bridge_class_name)],
        )
        .and_then(|value| value.l())
        .map(JClass::from)
        .map_err(|error| error.to_string())?;
    let updated = match env
        .new_object(
            &bridge_class,
            "(Landroid/content/Context;)V",
            &[JValue::Object(&activity)],
        )
        .and_then(|updater| env.call_method(updater, "runUpdate", "()Z", &[]))
        .and_then(|value| value.z())
    {
        Ok(updated) => updated,
        Err(_) => {
            env.exception_describe().ok();
            env.exception_clear().map_err(|error| error.to_string())?;
            false
        }
    };

    if updated {
        return Ok("LibUpdateClient.runUpdate");
    }

    let sdk_dir = match env
        .call_static_method(
            &bridge_class,
            "getVrSdkLibPath",
            "(Landroid/content/Context;)Ljava/lang/String;",
            &[JValue::Object(&activity)],
        )
        .and_then(|value| value.l())
    {
        Ok(sdk_dir) => sdk_dir,
        Err(error) => {
            env.exception_describe().ok();
            env.exception_clear().map_err(|error| error.to_string())?;
            return Err(format!("getVrSdkLibPath failed: {error}"));
        }
    };
    if sdk_dir.is_null() {
        return Err("LibUpdateClient.getVrSdkLibPath() returned null".into());
    }

    let sdk_dir: String = env
        .get_string(&JString::from(sdk_dir))
        .map_err(|error| error.to_string())?
        .into();
    let runtime_path = format!("{}/libvrsdk.so", sdk_dir.trim_end_matches('/'));
    let runtime_path = JObject::from(
        env.new_string(runtime_path)
            .map_err(|error| error.to_string())?,
    );
    if let Err(error) = env.call_static_method(
        "java/lang/System",
        "load",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&runtime_path)],
    ) {
        env.exception_describe().ok();
        env.exception_clear().map_err(|error| error.to_string())?;
        return Err(format!("System.load(libvrsdk.so) failed: {error}"));
    }

    Ok("System.load(libvrsdk.so)")
}
