// Required by napi-build to generate platform-specific .def / linker files
// for the native Node.js addon (.node output).
extern crate napi_build;

fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let sdk_root = manifest_dir.join("../../external/vst3sdk");
    let ara_root = manifest_dir.join("../../external/ARA_SDK");
    let bridge_root = manifest_dir.join("vst3bridge");

    // Trigger rebuilds when any bridge source or header changes.
    for name in &[
        "include/sphere_daux_vst3_processor.h",
        "include/sphere_daux_editor_bridge.h",
        "include/editor_windows.hpp",
        "src/vst3_processor.cpp",
        "src/editor_windows.cpp",
        "src/editorplatform/windows/editor_windows_api.cpp",
        "src/editorplatform/windows/editor_windows_common.cpp",
        "src/editorplatform/windows/editor_windows_create.cpp",
        "src/editorplatform/windows/editor_windows_internal.hpp",
        "src/editorplatform/windows/editor_windows_rendering.cpp",
        "src/editorplatform/windows/editor_windows_titlebar.cpp",
        "src/editorplatform/windows/editor_windows_utils.cpp",
        "src/editorplatform/windows/editor_windows_windowproc.cpp",
        "src/editorplatform/macos/editor_mac.mm",
        "src/editorplatform/macos/editor_mac_delegate.mm",
        "src/editorplatform/macos/editor_mac_helpers.mm",
        "src/editorplatform/macos/editor_mac_shell.mm",
        "src/editorplatform/macos/editor_mac_internal.hpp",
        "src/editor_chrome_stub.cpp",
        "src/editor_linux.cpp",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            bridge_root.join(name).display()
        );
    }

    // Baseline x64 VST3 bridge — no /arch:AVX2 or target-cpu=native.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .flag_if_supported("/Zc:char8_t-")
        .flag_if_supported("/EHsc")
        .include(bridge_root.join("include"))
        .include(&sdk_root)
        .include(sdk_root.join("pluginterfaces"))
        .include(sdk_root.join("public.sdk/source"))
        .file(bridge_root.join("src/vst3_processor.cpp"))
        .file(bridge_root.join("src/editor_windows.cpp"))
        .file(sdk_root.join("pluginterfaces/base/coreiids.cpp"))
        .file(sdk_root.join("pluginterfaces/base/funknown.cpp"))
        .file(sdk_root.join("pluginterfaces/base/ustring.cpp"))
        .file(sdk_root.join("public.sdk/source/common/commonstringconvert.cpp"))
        .file(sdk_root.join("public.sdk/source/common/memorystream.cpp"))
        .file(sdk_root.join("public.sdk/source/vst/utility/stringconvert.cpp"))
        .file(sdk_root.join("public.sdk/source/vst/vstinitiids.cpp"))
        .file(sdk_root.join("public.sdk/source/vst/hosting/hostclasses.cpp"))
        .file(sdk_root.join("public.sdk/source/vst/hosting/pluginterfacesupport.cpp"))
        .file(sdk_root.join("public.sdk/source/vst/hosting/module.cpp"));

    add_optional_ara_include(&mut build, &ara_root);
    apply_vst3_platform_config(&mut build, &sdk_root, &bridge_root);

    build.compile("sphere_daux_vst3_processor");

    build_vst2_bridge(&manifest_dir, &bridge_root);
    build_clap_bridge(&manifest_dir, &bridge_root);

    napi_build::setup();
}

/// Add the header-only ARA API include path, but only when the SDK submodule is
/// actually checked out.
///
/// `external/ARA_SDK` is optional: ARA *hosting* is a Windows/macOS feature
/// (`SphereAraHost` is a per-target dependency) and CI does not initialize the
/// submodule, while this bridge builds on every platform. The C++ takes
/// `kARAMainFactoryClass` from the header when this path resolves and falls
/// back to the same published literal when it does not, so a missing submodule
/// changes nothing about how ARA entry points are located — it must not fail
/// the build.
fn add_optional_ara_include(build: &mut cc::Build, ara_root: &std::path::Path) {
    let api = ara_root.join("ARA_API");
    println!("cargo:rerun-if-changed={}", api.join("ARAVST3.h").display());
    if api.join("ARAVST3.h").is_file() {
        build.include(api);
    } else {
        println!(
            "cargo:warning=ARA SDK not checked out at {}; building without it \
             (ARA entry points are still located by class category)",
            ara_root.display()
        );
    }
}

/// CLAP runtime bridge. Like the VST2 bridge it is its own static lib: it
/// shares no Steinberg SDK sources with the VST3 bridge, only the
/// platform-neutral native editor shell (`editor_windows.hpp`), whose
/// implementation the VST3 build already links in.
fn build_clap_bridge(manifest_dir: &std::path::Path, vst3_bridge_root: &std::path::Path) {
    let root = manifest_dir.join("clapbridge");
    let clap_root = manifest_dir.join("../../external/clap");

    for name in &[
        "include/sphere_daux_clap_processor.h",
        "include/clap_processor_internal.hpp",
        "src/clap_processor.cpp",
        "src/clap_editor_windows.cpp",
        "src/clap_editor_mac.mm",
        "src/clap_editor_stub.cpp",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(name).display());
    }

    // Baseline x64 — no /arch:AVX2 or target-cpu=native, same as the other
    // bridges: the distributed plugin host must run on CPUs without AVX2.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .flag_if_supported("/Zc:char8_t-")
        .flag_if_supported("/EHsc")
        .include(root.join("include"))
        .include(clap_root.join("include"))
        // For editor_windows.hpp — the shared native editor shell.
        .include(vst3_bridge_root.join("include"))
        .file(root.join("src/clap_processor.cpp"));

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => {
            build.file(root.join("src/clap_editor_windows.cpp"));
        }
        "macos" => {
            build
                .flag("-fobjc-arc")
                .file(root.join("src/clap_editor_mac.mm"));
        }
        _ => {
            // Linux and anything else: CLAP plug-ins still load and process;
            // only the embedded editor is stubbed out.
            build.file(root.join("src/clap_editor_stub.cpp"));
        }
    }

    build.compile("sphere_daux_clap_processor");
}

/// VST2 runtime bridge. Built as its own static lib because it shares no
/// Steinberg SDK sources with the VST3 bridge — only the platform-neutral
/// native editor shell (`editor_windows.hpp`), whose implementation is already
/// linked in by the VST3 build above.
fn build_vst2_bridge(manifest_dir: &std::path::Path, vst3_bridge_root: &std::path::Path) {
    let root = manifest_dir.join("vst2bridge");

    for name in &[
        "include/sphere_vst2_abi.h",
        "include/sphere_daux_vst2_processor.h",
        "include/vst2_processor_internal.hpp",
        "src/vst2_processor.cpp",
        "src/vst2_editor_windows.cpp",
        "src/vst2_editor_mac.mm",
        "src/vst2_editor_stub.cpp",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(name).display());
    }

    // Baseline x64 — no /arch:AVX2 or target-cpu=native, same as the VST3
    // bridge: the distributed plugin host must run on CPUs without AVX2.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .flag_if_supported("/Zc:char8_t-")
        .flag_if_supported("/EHsc")
        .include(root.join("include"))
        // For editor_windows.hpp — the shared native editor shell.
        .include(vst3_bridge_root.join("include"))
        .file(root.join("src/vst2_processor.cpp"));

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => {
            build.file(root.join("src/vst2_editor_windows.cpp"));
        }
        "macos" => {
            build
                .flag("-fobjc-arc")
                .file(root.join("src/vst2_editor_mac.mm"));
        }
        _ => {
            // Linux and anything else: create() reports "unsupported platform"
            // and the editor entry points are inert stubs.
            build.file(root.join("src/vst2_editor_stub.cpp"));
        }
    }

    build.compile("sphere_daux_vst2_processor");
}

fn apply_vst3_platform_config(
    build: &mut cc::Build,
    sdk_root: &std::path::Path,
    bridge_root: &std::path::Path,
) {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // The editor chrome strip's no-op half. Compiled on every target and empty
    // on macOS, where `editor_mac_shell.mm` provides the real one — one file in
    // the build list rather than a platform branch that has to be kept in step
    // with the one below.
    build.file(bridge_root.join("src/editor_chrome_stub.cpp"));

    match target_os.as_str() {
        "windows" => {
            build.define("SMTG_OS_WINDOWS", "1");
            build.file(sdk_root.join("public.sdk/source/vst/hosting/module_win32.cpp"));
            for source in &[
                "src/editorplatform/windows/editor_windows_api.cpp",
                "src/editorplatform/windows/editor_windows_common.cpp",
                "src/editorplatform/windows/editor_windows_create.cpp",
                "src/editorplatform/windows/editor_windows_rendering.cpp",
                "src/editorplatform/windows/editor_windows_titlebar.cpp",
                "src/editorplatform/windows/editor_windows_utils.cpp",
                "src/editorplatform/windows/editor_windows_windowproc.cpp",
            ] {
                build.file(bridge_root.join(source));
            }
            println!("cargo:rustc-link-lib=ole32");
            println!("cargo:rustc-link-lib=user32");
            println!("cargo:rustc-link-lib=gdi32");
            println!("cargo:rustc-link-lib=dwmapi");
            println!("cargo:rustc-link-lib=dwrite");
        }
        "macos" => {
            build.define("SMTG_OS_MACOS", "1");
            build.flag("-fobjc-arc");
            build.file(sdk_root.join("public.sdk/source/vst/hosting/module_mac.mm"));
            for source in &[
                "src/editorplatform/macos/editor_mac.mm",
                "src/editorplatform/macos/editor_mac_delegate.mm",
                "src/editorplatform/macos/editor_mac_helpers.mm",
                "src/editorplatform/macos/editor_mac_shell.mm",
            ] {
                build.file(bridge_root.join(source));
            }
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=Foundation");
            println!("cargo:rustc-link-lib=framework=AppKit");
        }
        "linux" => {
            build.define("SMTG_OS_LINUX", "1");
            build.file(sdk_root.join("public.sdk/source/vst/hosting/module_linux.cpp"));
            build.file(bridge_root.join("src/editor_linux.cpp"));

            let gtk4 = pkg_config::probe_library("gtk4").expect(
                "GTK4 not found — install libgtk-4-dev (Debian/Ubuntu) or gtk4-devel (Fedora)",
            );
            for path in &gtk4.include_paths {
                build.include(path);
            }
            for (key, val) in &gtk4.defines {
                build.define(key, val.as_deref());
            }

            println!("cargo:rustc-link-lib=dl");
            // editor_linux.cpp uses XGrabKey so Space reaches the host even when
            // an XEmbed plug-in child holds focus.
            println!("cargo:rustc-link-lib=X11");
        }
        _ => {}
    }
}
