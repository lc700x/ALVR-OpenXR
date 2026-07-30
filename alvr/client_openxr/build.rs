use std::{env, path::PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let sdk_include = crate_dir
        .join("../../build/temp_download/vr-sdks/QiyuNativeSDK-v1.0.0/QiyuNativeSDK/Include");
    let sdk_libs = crate_dir.join("../../deps/android_openxr/arm64-v8a");

    println!("cargo:rerun-if-changed=src/qiyu_backend.cpp");
    println!("cargo:rerun-if-changed={}", sdk_include.display());
    println!("cargo:rustc-link-search=native={}", sdk_libs.display());
    println!("cargo:rustc-link-lib=dylib=dl");

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("src/qiyu_backend.cpp")
        .include("src")
        .include(sdk_include)
        .compile("alvr_qiyu_backend");
}
