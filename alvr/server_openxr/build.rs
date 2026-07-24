use std::{env, path::PathBuf};

fn deps_windows() -> PathBuf {
    alvr_filesystem::deps_dir().join("windows")
}

fn main() {
    let platform_name = env::var("CARGO_CFG_TARGET_OS").unwrap();
    if platform_name != "windows" {
        // The OpenXR runtime is Windows-only for now (blueprint decision Q3).
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let openvr_cpp = manifest_dir.join("../server_openvr/cpp");
    let win32 = openvr_cpp.join("platform/win32");

    let gpl = cfg!(feature = "gpl");

    // Curated subset: the video encoder + FrameRender pipeline, excluding the
    // SteamVR driver translation units (alvr_server.cpp, HMD/Controller/etc.).
    let mut sources = vec![
        manifest_dir.join("cpp/glue.cpp"),
        win32.join("CEncoder.cpp"),
        win32.join("FrameRender.cpp"),
        win32.join("FFR.cpp"),
        win32.join("VideoEncoder.cpp"),
        win32.join("VideoEncoderNVENC.cpp"),
        win32.join("NvEncoder.cpp"),
        win32.join("NvEncoderD3D11.cpp"),
        win32.join("VideoEncoderAMF.cpp"),
        // VideoEncoderVPL.cpp was added after v20 — not present in this branch.
        openvr_cpp.join("alvr_server/NalParsing.cpp"),
        openvr_cpp.join("alvr_server/IDRScheduler.cpp"),
        openvr_cpp.join("alvr_server/Logger.cpp"),
        openvr_cpp.join("alvr_server/driverlog.cpp"),
        openvr_cpp.join("alvr_server/Settings.cpp"), // provides Settings::m_Instance and Load()
        openvr_cpp.join("shared/threadtools.cpp"),
        openvr_cpp.join("ALVR-common/exception.cpp"),
        openvr_cpp.join("ALVR-common/common-utils.cpp"),
    ];
    for entry in walkdir::WalkDir::new(win32.join("d3d-render-utils"))
        .into_iter()
        .chain(walkdir::WalkDir::new(win32.join("shared")))
        .chain(walkdir::WalkDir::new(openvr_cpp.join("shared/amf")))
        .filter_map(|e| e.ok())
    {
        let path = entry.into_path();
        if path.extension().is_some_and(|ext| ext == "cpp") {
            sources.push(path);
        }
    }
    if gpl {
        sources.push(win32.join("VideoEncoderSW.cpp"));
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .files(&sources)
        .include(alvr_filesystem::workspace_dir().join("openvr/headers"))
        .include(&openvr_cpp)
        .debug(false)
        .flag("/permissive-")
        // /EHa: catch SEH (access violations) via C++ catch(...), so a fault in
        // the reused encoder/FrameRender init fails gracefully instead of
        // terminating the host OpenXR app.
        .flag("/EHa")
        .define("NOMINMAX", None)
        .define("_WINSOCKAPI_", None)
        .define("_MBCS", None)
        .define("_MT", None);

    #[cfg(debug_assertions)]
    build.define("ALVR_DEBUG_LOG", None);

    let vulkan_headers = deps_windows().join("vulkan-headers/include");
    assert!(
        vulkan_headers.exists(),
        "run: cargo xtask prepare-deps --platform windows"
    );
    build.include(vulkan_headers);

    if gpl {
        let ffmpeg = deps_windows().join("ffmpeg");
        assert!(ffmpeg.join("include").exists());
        build.include(ffmpeg.join("include"));
        build.define("ALVR_GPL", None);
        // FFmpeg 6+ removed the FF_PROFILE_H264_* aliases; define compat shims
        // so that v20-era VideoEncoderSW.cpp builds with newer FFmpeg headers.
        build.define("FF_PROFILE_H264_BASELINE", "66");
        build.define("FF_PROFILE_H264_MAIN", "77");
        build.define("FF_PROFILE_H264_HIGH", "100");
        build.define("FF_PROFILE_H264_HIGH_10", "110");
        build.define("FF_PROFILE_H264_HIGH_422", "122");
        build.define("FF_PROFILE_H264_HIGH_444", "144");
    }

    let vpl = deps_windows().join("libvpl/alvr_build");
    let vpl_source = win32.join("VideoEncoderVPL.cpp");
    if vpl_source.exists() && vpl.join("include").exists() {
        // VPL was added after v20.14.0; compile it only when both the source and
        // the library headers are present.
        sources.push(vpl_source);
        build.define("ONEVPL_EXPERIMENTAL", None);
        build.include(vpl.join("include"));
        println!(
            "cargo:rustc-link-search=native={}",
            vpl.join("lib").to_string_lossy()
        );
        println!("cargo:rustc-link-lib=vpl");
    }

    build.compile("alvr_openxr_encoder");

    if gpl {
        let ffmpeg_lib = deps_windows().join("ffmpeg/lib");
        assert!(ffmpeg_lib.exists());
        println!(
            "cargo:rustc-link-search=native={}",
            ffmpeg_lib.to_string_lossy()
        );
        for lib in ["avutil", "avfilter", "avcodec", "swscale"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }

    bindgen::builder()
        .clang_arg("-xc++")
        .clang_arg(format!("-I{}", openvr_cpp.to_string_lossy()))
        .clang_arg(format!(
            "-I{}",
            alvr_filesystem::workspace_dir()
                .join("openvr/headers")
                .to_string_lossy()
        ))
        .header(openvr_cpp.join("alvr_server/bindings.h").to_string_lossy())
        .derive_default(true)
        .generate()
        .unwrap()
        .write_to_file(out_dir.join("bindings.rs"))
        .unwrap();

    for path in &sources {
        println!("cargo:rerun-if-changed={}", path.to_string_lossy());
    }
}
