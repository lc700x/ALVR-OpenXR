use crate::command;
use alvr_filesystem::{self as afs, Layout};
use std::{
    env,
    fmt::{self, Display, Formatter},
    fs,
    path::{Path, PathBuf},
    vec,
};
use xshell::{cmd, Shell};

#[derive(Clone, Copy)]
pub enum Profile {
    Debug,
    Release,
    Distribution,
}

impl Display for Profile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let string = match self {
            Profile::Distribution => "distribution",
            Profile::Release => "release",
            Profile::Debug => "debug",
        };
        write!(f, "{string}")
    }
}

pub fn build_server_lib(profile: Profile, root: Option<String>, reproducible: bool) {
    let sh = Shell::new().unwrap();

    let mut flags = vec![];
    match profile {
        Profile::Distribution => {
            flags.push("--profile");
            flags.push("distribution");
        }
        Profile::Release => flags.push("--release"),
        Profile::Debug => (),
    }
    if reproducible {
        flags.push("--locked");
    }
    let flags_ref = &flags;

    let artifacts_dir = afs::target_dir().join(profile.to_string());

    let build_dir = afs::build_dir().join("alvr_server_core");
    sh.create_dir(&build_dir).unwrap();

    if let Some(root) = root {
        sh.set_var("ALVR_ROOT_DIR", root);
    }

    let _push_guard = sh.push_dir(afs::crate_dir("server_core"));
    cmd!(sh, "cargo build {flags_ref...}").run().unwrap();

    sh.copy_file(
        artifacts_dir.join(afs::dynlib_fname("alvr_server_core")),
        &build_dir,
    )
    .unwrap();

    if cfg!(windows) {
        sh.copy_file(artifacts_dir.join("alvr_server_core.pdb"), &build_dir)
            .unwrap();
    }

    let out = build_dir.join("alvr_server_core.h");
    cmd!(sh, "cbindgen --output {out}").run().unwrap();
}

/// Build the standalone OpenXR runtime (no SteamVR) and assemble a package under
/// `build/alvr_openxr_runtime_[platform]` with the runtime DLL + native deps.
pub fn build_streamer_openxr(profile: Profile, gpl: bool, reproducible: bool) {
    let sh = Shell::new().unwrap();

    let mut flags = vec![];
    match profile {
        Profile::Distribution => {
            flags.push("--profile");
            flags.push("distribution");
        }
        Profile::Release => flags.push("--release"),
        Profile::Debug => (),
    }
    if reproducible {
        flags.push("--locked");
    }
    let gpl_flag = if gpl {
        vec!["--features", "gpl"]
    } else {
        vec![]
    };
    let flags_ref = &flags;

    let artifacts_dir = afs::target_dir().join(profile.to_string());
    let build_dir = afs::build_dir().join("alvr_openxr_runtime_windows");
    let bin_dir = build_dir.join("bin/win64");

    sh.remove_path(&build_dir).ok();
    sh.create_dir(&bin_dir).unwrap();

    {
        let _push_guard = sh.push_dir(afs::crate_dir("server_openxr"));
        cmd!(sh, "cargo build {flags_ref...} {gpl_flag...}")
            .run()
            .unwrap();
    }

    sh.copy_file(
        artifacts_dir.join(afs::dynlib_fname("alvr_server_openxr")),
        &bin_dir,
    )
    .unwrap();

    // Registration helper (self-elevating). Placed next to the DLL.
    sh.copy_file(
        artifacts_dir.join(afs::exec_fname("alvr_openxr_register")),
        &bin_dir,
    )
    .unwrap();

    // Persistent server_core host. Keeps headset connection alive when the
    // OpenXR app (and therefore the runtime DLL process) exits.
    sh.copy_file(
        artifacts_dir.join(afs::exec_fname("alvr_openxr_host")),
        &bin_dir,
    )
    .unwrap();

    // Dashboard, placed next to the DLL so it shares session.json and can
    // auto-run the registration helper on startup.
    {
        let _push_guard = sh.push_dir(afs::crate_dir("dashboard"));
        cmd!(sh, "cargo build {flags_ref...}").run().unwrap();
    }
    sh.copy_file(
        artifacts_dir.join(afs::exec_fname("alvr_dashboard")),
        &bin_dir,
    )
    .unwrap();

    // Native encoder deps (mirrors the streamer package layout).
    if gpl {
        for lib_path in sh
            .read_dir(afs::deps_dir().join("windows/ffmpeg/bin"))
            .unwrap()
            .into_iter()
            .filter(|path| path.extension().is_some_and(|e| e == "dll"))
        {
            sh.copy_file(lib_path, &bin_dir).unwrap();
        }
    }
    // Intel VPL (Quick Sync) is optional and not part of the v20 encoder set
    // (VideoEncoderVPL.cpp is absent, so the runtime never links libvpl). Only
    // bundle the DLL when it was actually fetched; skip otherwise.
    let libvpl_dll = afs::deps_dir().join("windows/libvpl/alvr_build/bin/libvpl.dll");
    if libvpl_dll.exists() {
        sh.copy_file(libvpl_dll, &bin_dir).unwrap();
    }
}

pub fn build_streamer(
    profile: Profile,
    gpl: bool,
    root: Option<String>,
    reproducible: bool,
    profiling: bool,
    keep_config: bool,
) {
    let sh = Shell::new().unwrap();

    let build_layout = Layout::new(&afs::streamer_build_dir());

    let mut common_flags = vec![];
    match profile {
        Profile::Distribution => {
            common_flags.push("--profile");
            common_flags.push("distribution");
        }
        Profile::Release => common_flags.push("--release"),
        Profile::Debug => (),
    }
    if reproducible {
        common_flags.push("--locked");
    }

    let artifacts_dir = if cfg!(all(windows, target_arch = "aarch64")) {
        // Fix for cross compilation
        const TARGET: &str = "x86_64-pc-windows-msvc";

        common_flags.push("--target");
        common_flags.push(TARGET);

        afs::target_dir().join(TARGET).join(profile.to_string())
    } else {
        afs::target_dir().join(profile.to_string())
    };

    let common_flags_ref = &common_flags;

    let maybe_config = if keep_config {
        fs::read_to_string(build_layout.session()).ok()
    } else {
        None
    };

    sh.remove_path(afs::streamer_build_dir()).ok();
    sh.create_dir(build_layout.openvr_driver_lib_dir()).unwrap();
    sh.create_dir(&build_layout.executables_dir).unwrap();

    if let Some(config) = maybe_config {
        fs::write(build_layout.session(), config).ok();
    }

    if let Some(root) = root {
        sh.set_var("ALVR_ROOT_DIR", root);
    }

    // build server
    {
        let gpl_flag = if gpl {
            vec!["--features", "gpl"]
        } else {
            vec![]
        };

        let profiling_flag = if profiling {
            vec!["--features", "alvr_server_core/trace-performance"]
        } else {
            vec![]
        };

        let _push_guard = sh.push_dir(afs::crate_dir("server_openvr"));
        cmd!(
            sh,
            "cargo build {common_flags_ref...} {gpl_flag...} {profiling_flag...}"
        )
        .run()
        .unwrap();

        sh.copy_file(
            artifacts_dir.join(afs::dynlib_fname("alvr_server_openvr")),
            build_layout.openvr_driver_lib(),
        )
        .unwrap();

        if cfg!(windows) {
            sh.copy_file(
                artifacts_dir.join("alvr_server_openvr.pdb"),
                build_layout
                    .openvr_driver_lib_dir()
                    .join("alvr_server_openvr.pdb"),
            )
            .unwrap();
        }
    }

    // Build dashboard
    {
        let _push_guard = sh.push_dir(afs::crate_dir("dashboard"));
        cmd!(sh, "cargo build {common_flags_ref...}").run().unwrap();

        sh.copy_file(
            artifacts_dir.join(afs::exec_fname("alvr_dashboard")),
            build_layout.dashboard_exe(),
        )
        .unwrap();
    }

    // copy dependencies
    if cfg!(windows) {
        sh.copy_file(
            afs::workspace_dir().join("openvr/bin/win64/openvr_api.dll"),
            build_layout.openvr_driver_lib_dir(),
        )
        .unwrap();

        // Bring along the c++ runtime
        command::copy_recursive(
            &sh,
            &afs::crate_dir("server_openvr").join("cpp/bin/windows"),
            &build_layout.openvr_driver_lib_dir(),
        )
        .unwrap();

        // copy ffmpeg binaries
        if gpl {
            let bin_dir = &build_layout.openvr_driver_lib_dir();
            sh.create_dir(bin_dir).unwrap();
            for lib_path in sh
                .read_dir(afs::deps_dir().join("windows/ffmpeg/bin"))
                .unwrap()
                .into_iter()
                .filter(|path| path.file_name().unwrap().to_string_lossy().contains(".dll"))
            {
                sh.copy_file(lib_path.clone(), bin_dir).unwrap();
            }
        }
    } else if cfg!(target_os = "linux") {
        // build compositor wrapper
        let _push_guard = sh.push_dir(afs::crate_dir("vrcompositor_wrapper"));
        cmd!(sh, "cargo build {common_flags_ref...}").run().unwrap();
        sh.create_dir(&build_layout.vrcompositor_wrapper_dir)
            .unwrap();
        sh.copy_file(
            artifacts_dir.join("alvr_vrcompositor_wrapper"),
            build_layout.vrcompositor_wrapper(),
        )
        .unwrap();
        sh.copy_file(
            artifacts_dir.join("alvr_drm_lease_shim.so"),
            build_layout.drm_lease_shim(),
        )
        .unwrap();

        // build vulkan layer
        let _push_guard = sh.push_dir(afs::crate_dir("vulkan_layer"));
        cmd!(sh, "cargo build {common_flags_ref...}").run().unwrap();
        sh.create_dir(&build_layout.libraries_dir).unwrap();
        sh.copy_file(
            artifacts_dir.join(afs::dynlib_fname("alvr_vulkan_layer")),
            build_layout.vulkan_layer(),
        )
        .unwrap();

        // copy vulkan layer manifest
        sh.create_dir(&build_layout.vulkan_layer_manifest_dir)
            .unwrap();
        sh.copy_file(
            afs::crate_dir("vulkan_layer").join("layer/alvr_x86_64.json"),
            build_layout.vulkan_layer_manifest(),
        )
        .unwrap();

        sh.copy_file(
            afs::workspace_dir().join("openvr/bin/linux64/libopenvr_api.so"),
            build_layout.openvr_driver_lib_dir(),
        )
        .unwrap();

        let firewall_script = afs::crate_dir("xtask").join("firewall/alvr_fw_config.sh");
        let firewalld = afs::crate_dir("xtask").join("firewall/alvr-firewalld.xml");
        let ufw = afs::crate_dir("xtask").join("firewall/ufw-alvr");

        // copy linux specific firewalls
        sh.copy_file(firewall_script, build_layout.firewall_script())
            .unwrap();
        sh.copy_file(firewalld, build_layout.firewalld_config())
            .unwrap();
        sh.copy_file(ufw, build_layout.ufw_config()).unwrap();
    }

    // copy static resources
    {
        // copy driver manifest
        sh.copy_file(
            afs::crate_dir("xtask").join("resources/driver.vrdrivermanifest"),
            build_layout.openvr_driver_manifest(),
        )
        .unwrap();
    }
}

pub fn build_launcher(profile: Profile, reproducible: bool) {
    let sh = Shell::new().unwrap();

    let mut common_flags = vec![];
    match profile {
        Profile::Distribution => {
            common_flags.push("--profile");
            common_flags.push("distribution");
        }
        Profile::Release => common_flags.push("--release"),
        Profile::Debug => (),
    }
    if reproducible {
        common_flags.push("--locked");
    }
    let common_flags_ref = &common_flags;

    sh.create_dir(afs::launcher_build_dir()).unwrap();

    cmd!(sh, "cargo build -p alvr_launcher {common_flags_ref...}")
        .run()
        .unwrap();

    sh.copy_file(
        afs::target_dir()
            .join(profile.to_string())
            .join(afs::exec_fname("alvr_launcher")),
        afs::launcher_build_exe_path(),
    )
    .unwrap();
}

fn build_android_lib_impl(dir_name: &str, profile: Profile, link_stdcpp: bool, all_targets: bool) {
    let sh = Shell::new().unwrap();

    let mut ndk_flags = vec!["--platform", "26", "-t", "arm64-v8a"];

    if all_targets {
        ndk_flags.extend(["-t", "armeabi-v7a", "-t", "x86_64", "-t", "x86"]);
    }

    let mut rust_flags = vec![];
    match profile {
        Profile::Distribution => {
            rust_flags.push("--profile");
            rust_flags.push("distribution")
        }
        Profile::Release => rust_flags.push("--release"),
        Profile::Debug => (),
    }
    if !link_stdcpp {
        rust_flags.push("--no-default-features");
    }
    let rust_flags_ref = &rust_flags;

    let build_dir = afs::build_dir().join(format!("alvr_{dir_name}"));
    sh.create_dir(&build_dir).unwrap();

    let _push_guard = sh.push_dir(afs::crate_dir(dir_name));
    cmd!(
        sh,
        "cargo ndk {ndk_flags...} -o {build_dir} build {rust_flags_ref...}"
    )
    .run()
    .unwrap();

    let out = build_dir.join(format!("alvr_{dir_name}.h"));
    cmd!(sh, "cbindgen --output {out}").run().unwrap();
}

pub fn build_android_client_core_lib(profile: Profile, link_stdcpp: bool, all_targets: bool) {
    build_android_lib_impl("client_core", profile, link_stdcpp, all_targets)
}

pub fn build_android_client_openxr_lib(profile: Profile, link_stdcpp: bool) {
    build_android_lib_impl("client_openxr", profile, link_stdcpp, false)
}

fn add_android_vendor_runtime(sh: &Shell, apk_path: &Path, profile: Profile, keystore_path: &Path) {
    let sdk_lib_dir = afs::workspace_dir()
        .join("build/temp_download/vr-sdks/QiyuNativeSDK-v1.0.0/QiyuNativeSDK/Lib");
    if !sdk_lib_dir.exists() {
        panic!("QiYu SDK not found at {}", sdk_lib_dir.display());
    }

    let variant = if matches!(profile, Profile::Debug) {
        "debug"
    } else {
        "release"
    };
    let aar_names = ["sxrApi", "qiyivrsdkcore", "qiyuapi"];
    let work_dir = afs::build_dir().join("alvr_client_android/qiyu_java");
    sh.remove_path(&work_dir).ok();
    sh.create_dir(&work_dir).unwrap();

    let mut jars = Vec::new();
    for name in aar_names {
        let aar_path = sdk_lib_dir.join(format!("{name}-v8a-{variant}.aar"));
        let extract_dir = work_dir.join(name);
        sh.create_dir(&extract_dir).unwrap();
        cmd!(sh, "tar -xf {aar_path} -C {extract_dir} classes.jar")
            .run()
            .unwrap();
        jars.push(extract_dir.join("classes.jar"));
    }

    let hvr_bridge = afs::workspace_dir()
        .join("build/temp_download/vr-sdks/HVRsdkDemo/openXRsdk/libs/hvrbridge.jar");
    if !hvr_bridge.exists() {
        panic!("Huawei VR bridge not found at {}", hvr_bridge.display());
    }
    jars.push(hvr_bridge);

    let core_aar = sdk_lib_dir.join(format!("qiyivrsdkcore-v8a-{variant}.aar"));
    let asset_extract_dir = work_dir.join("asset_extract");
    sh.create_dir(&asset_extract_dir).unwrap();
    cmd!(
        sh,
        "tar -xf {core_aar} -C {asset_extract_dir} assets/qiyu_recenter.png"
    )
    .run()
    .unwrap();

    let android_home = PathBuf::from(env::var("ANDROID_HOME").expect("ANDROID_HOME not set"));
    let build_tools_dir = android_home.join("build-tools/32.0.0");
    let java = PathBuf::from(env::var("JAVA_HOME").expect("JAVA_HOME not set"))
        .join("bin")
        .join(afs::exec_fname("java"));
    let d8_jar = build_tools_dir.join("lib/d8.jar");
    let aapt = build_tools_dir.join(afs::exec_fname("aapt"));
    let zipalign = build_tools_dir.join(afs::exec_fname("zipalign"));
    let apksigner = build_tools_dir.join(if cfg!(windows) {
        "apksigner.bat"
    } else {
        "apksigner"
    });
    let android_jar = android_home.join("platforms/android-32/android.jar");

    // Compile the QiYu VRActivity Java class and add it to the dex inputs. The SXR SDK
    // (qiyu_Init -> QVR_Init) must be driven from a real Activity; VRActivity forwards to
    // the JNI bridge in libalvr_client_openxr.so.
    let java_home = PathBuf::from(env::var("JAVA_HOME").expect("JAVA_HOME not set"));
    let javac = java_home.join("bin").join(afs::exec_fname("javac"));
    let jar_tool = java_home.join("bin").join(afs::exec_fname("jar"));
    let vractivity_src = afs::workspace_dir()
        .join("alvr/client_openxr/android/java/alvr/client/VRActivity.java");
    let vractivity_classes_dir = work_dir.join("vractivity_classes");
    sh.create_dir(&vractivity_classes_dir).unwrap();
    cmd!(
        sh,
        "{javac} -source 8 -target 8 -classpath {android_jar} -d {vractivity_classes_dir} {vractivity_src}"
    )
    .run()
    .unwrap();
    let vractivity_jar = work_dir.join("vractivity.jar");
    cmd!(sh, "{jar_tool} cf {vractivity_jar} -C {vractivity_classes_dir} .")
        .run()
        .unwrap();
    jars.push(vractivity_jar);

    let dex_dir = work_dir.join("dex");
    sh.create_dir(&dex_dir).unwrap();
    let jars_ref = &jars;
    cmd!(
        sh,
        "{java} -cp {d8_jar} com.android.tools.r8.D8 --min-api 26 --lib {android_jar} --output {dex_dir} {jars_ref...}"
    )
    .run()
    .unwrap();

    let package_dir = work_dir.join("package");
    let assets_dir = package_dir.join("assets");
    sh.create_dir(&assets_dir).unwrap();
    sh.copy_file(dex_dir.join("classes.dex"), package_dir.join("classes.dex"))
        .unwrap();
    sh.copy_file(
        asset_extract_dir.join("assets/qiyu_recenter.png"),
        assets_dir.join("qiyu_recenter.png"),
    )
    .unwrap();

    {
        let _push_guard = sh.push_dir(&package_dir);
        cmd!(
            sh,
            "{aapt} remove {apk_path} classes.dex assets/qiyu_recenter.png"
        )
        .run()
        .ok();
        cmd!(
            sh,
            "{aapt} add {apk_path} classes.dex assets/qiyu_recenter.png"
        )
        .run()
        .unwrap();
    }

    let aligned_apk = work_dir.join("alvr_client_qiyu_aligned.apk");
    let signed_apk = work_dir.join("alvr_client_qiyu_signed.apk");
    cmd!(sh, "{zipalign} -f 4 {apk_path} {aligned_apk}")
        .run()
        .unwrap();
    cmd!(sh, "{apksigner} sign --ks {keystore_path} --ks-pass pass:alvrclient --out {signed_apk} {aligned_apk}")
        .run()
        .unwrap();

    // Activate the QiYu VRActivity as the launcher via apktool (the binary AndroidManifest.xml
    // cannot be edited with aapt). VRActivity delegates to NativeActivity on non-QiYu devices,
    // so this remains a single universal APK. Skipped (with a warning) if apktool.jar is absent.
    let apktool_jar = afs::workspace_dir().join("build/tools/apktool.jar");
    if apktool_jar.exists() {
        let decoded_dir = work_dir.join("apktool_decoded");
        sh.remove_path(&decoded_dir).ok();
        cmd!(
            sh,
            "{java} -jar {apktool_jar} d -s -f {signed_apk} -o {decoded_dir}"
        )
        .run()
        .unwrap();

        let manifest_path = decoded_dir.join("AndroidManifest.xml");
        let manifest = fs::read_to_string(&manifest_path).unwrap();
        let native_activity_open = concat!(
            "<activity android:configChanges=\"density|keyboard|keyboardHidden|navigation|",
            "orientation|screenLayout|screenSize|uiMode\" android:exported=\"true\" ",
            "android:launchMode=\"singleTask\" android:name=\"android.app.NativeActivity\" ",
            "android:screenOrientation=\"landscape\">"
        );
        // Insert a VRActivity launcher that mirrors the NativeActivity intent-filters, and demote
        // the NativeActivity so it no longer advertises itself as the launcher.
        let vractivity_block = concat!(
            "<activity android:configChanges=\"density|keyboard|keyboardHidden|navigation|",
            "orientation|screenLayout|screenSize|uiMode\" android:exported=\"true\" ",
            "android:launchMode=\"singleTask\" android:name=\"alvr.client.VRActivity\" ",
            "android:screenOrientation=\"landscape\">",
            "<intent-filter>",
            "<action android:name=\"android.intent.action.MAIN\"/>",
            "<category android:name=\"android.intent.category.LAUNCHER\"/>",
            "<category android:name=\"com.oculus.intent.category.VR\"/>",
            "<category android:name=\"com.yvr.intent.category.VR\"/>",
            "<category android:name=\"org.khronos.openxr.intent.category.IMMERSIVE_HMD\"/>",
            "</intent-filter>",
            "<intent-filter>",
            "<action android:name=\"com.huawei.android.vr.action.MAIN\"/>",
            "<category android:name=\"android.intent.category.DEFAULT\"/>",
            "</intent-filter>",
            "</activity>",
            "<activity android:configChanges=\"density|keyboard|keyboardHidden|navigation|",
            "orientation|screenLayout|screenSize|uiMode\" android:exported=\"true\" ",
            "android:launchMode=\"singleTask\" android:name=\"android.app.NativeActivity\" ",
            "android:screenOrientation=\"landscape\">"
        );
        assert!(
            manifest.contains(native_activity_open),
            "NativeActivity declaration not found in decoded manifest; apktool output changed"
        );
        // Replace only the opening tag: prepend VRActivity block, then strip NativeActivity's
        // launcher intent-filters (which now belong to VRActivity) while keeping the lib_name.
        let manifest = manifest.replacen(native_activity_open, vractivity_block, 1);

        // Both activities would otherwise advertise the same launcher and Huawei VR actions, and
        // an ambiguous implicit intent resolves to nothing on Huawei's VR launcher: it reports
        // "start activity success", no process starts, and the session ends on a dark screen.
        // Strip the filters structurally rather than by matching the serialized text, which is
        // whitespace- and line-ending-sensitive (apktool writes CRLF).
        let native_activity_start = manifest.rfind(native_activity_open).unwrap();
        let native_activity_end = manifest[native_activity_start..]
            .find("</activity>")
            .map(|offset| native_activity_start + offset)
            .expect("NativeActivity element is not closed; apktool output changed");
        let native_activity = &manifest[native_activity_start..native_activity_end];
        let mut stripped = String::with_capacity(native_activity.len());
        let mut rest = native_activity;
        let mut removed_filters = 0;
        while let Some(open) = rest.find("<intent-filter") {
            let close = rest[open..]
                .find("</intent-filter>")
                .map(|offset| open + offset + "</intent-filter>".len())
                .expect("unterminated intent-filter in NativeActivity");
            stripped.push_str(rest[..open].trim_end_matches([' ', '\t', '\r', '\n']));
            rest = &rest[close..];
            removed_filters += 1;
        }
        assert_eq!(
            removed_filters, 2,
            "expected the launcher and Huawei VR intent-filters on NativeActivity, found \
             {removed_filters}; apktool output changed"
        );
        stripped.push_str(rest);
        let manifest = format!(
            "{}{stripped}{}",
            &manifest[..native_activity_start],
            &manifest[native_activity_end..]
        );
        fs::write(&manifest_path, manifest).unwrap();

        let rebuilt_apk = work_dir.join("alvr_client_qiyu_vractivity_unsigned.apk");
        cmd!(
            sh,
            "{java} -jar {apktool_jar} b {decoded_dir} -o {rebuilt_apk} --use-aapt2"
        )
        .run()
        .unwrap();
        let vractivity_aligned = work_dir.join("alvr_client_qiyu_vractivity_aligned.apk");
        cmd!(sh, "{zipalign} -f 4 {rebuilt_apk} {vractivity_aligned}")
            .run()
            .unwrap();
        cmd!(sh, "{apksigner} sign --ks {keystore_path} --ks-pass pass:alvrclient --out {signed_apk} {vractivity_aligned}")
            .run()
            .unwrap();
    } else {
        println!(
            "cargo:warning=apktool.jar not found at {}; QiYu VRActivity launcher not activated \
             (A091 will use the OpenXR path without ABXY/thumbstick)",
            apktool_jar.display()
        );
    }

    fs::copy(signed_apk, apk_path).unwrap();
}

pub fn build_android_client(profile: Profile) {
    let sh = Shell::new().unwrap();

    let mut flags = vec![];
    match profile {
        Profile::Distribution => {
            flags.push("--profile");
            flags.push("distribution")
        }
        Profile::Release => flags.push("--release"),
        Profile::Debug => (),
    }
    let flags_ref = &flags;

    const ARTIFACT_NAME: &str = "alvr_client_android";

    let target_dir = afs::target_dir();
    let build_dir = afs::build_dir().join(ARTIFACT_NAME);
    sh.create_dir(&build_dir).unwrap();

    // Create debug keystore (signing will be overwritten by CI)
    if env::var(format!(
        "CARGO_APK_{}_KEYSTORE",
        profile.to_string().to_uppercase()
    ))
    .is_err()
        && matches!(profile, Profile::Release | Profile::Distribution)
    {
        let keystore_path = build_dir.join("debug.keystore");
        if !keystore_path.exists() {
            let keytool = PathBuf::from(env::var("JAVA_HOME").expect("Env var JAVA_HOME not set"))
                .join("bin")
                .join(afs::exec_fname("keytool"));
            let pass = "alvrclient";

            let other = vec![
                "-genkey",
                "-v",
                "-alias",
                "androiddebugkey",
                "-dname",
                "CN=Android Debug,O=Android,C=US",
                "-keyalg",
                "RSA",
                "-keysize",
                "2048",
                "-validity",
                "10000",
            ];

            cmd!(
                sh,
                "{keytool} -keystore {keystore_path} -storepass {pass} -keypass {pass} {other...}"
            )
            .run()
            .unwrap();
        }
    }

    let _push_guard = sh.push_dir(afs::crate_dir("client_openxr"));
    cmd!(
        sh,
        "cargo apk build --target-dir={target_dir} {flags_ref...}"
    )
    .run()
    .unwrap();

    let apk_path = afs::target_dir()
        .join(profile.to_string())
        .join("apk/alvr_client_openxr.apk");
    if !matches!(profile, Profile::Debug) {
        add_android_vendor_runtime(&sh, &apk_path, profile, &build_dir.join("debug.keystore"));
    }

    sh.copy_file(apk_path, build_dir.join(format!("{ARTIFACT_NAME}.apk")))
        .unwrap();
}
