fn main() {
    // Paths are relative to this crate's directory (crates/emufight).
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // Monorepo: workspace vendor/ymfm. Standalone publish: crate-local vendor/ymfm.
    let ymfm_candidates = [
        manifest_dir.join("../../vendor/ymfm/src"),
        manifest_dir.join("vendor/ymfm/src"),
    ];
    let ymfm_src = ymfm_candidates
        .into_iter()
        .find(|p| p.join("ymfm_adpcm.cpp").is_file())
        .unwrap_or_else(|| {
            panic!(
                "ymfm sources not found. Init the submodule: \
                 git submodule update --init --recursive (vendor/ymfm)"
            )
        });
    let glue_ym2610 = manifest_dir.join("src/ym2610_glue.cpp");
    let glue_ym2151 = manifest_dir.join("src/ym2151_glue.cpp");

    println!("cargo:rerun-if-changed={}", glue_ym2610.display());
    println!("cargo:rerun-if-changed={}", glue_ym2151.display());
    println!("cargo:rerun-if-changed={}", ymfm_src.display());

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "wasm32-unknown-unknown" {
        // No C++ ymfm build for WASM; the crate provides wasm stubs instead.
        return;
    }

    let mut build = cc::Build::new();
    build.cpp(true).pic(true).std("c++17");

    if target.contains("apple") {
        if let Ok(output) = std::process::Command::new("xcrun").arg("--show-sdk-path").output() {
            let sdk_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !sdk_path.is_empty() {
                build.include(format!("{}/usr/include/c++/v1", sdk_path));
            }
        }
    }

    build
        .file(ymfm_src.join("ymfm_adpcm.cpp"))
        .file(ymfm_src.join("ymfm_ssg.cpp"))
        .file(ymfm_src.join("ymfm_opn.cpp"))
        .file(ymfm_src.join("ymfm_opm.cpp"))
        .file(ymfm_src.join("ymfm_pcm.cpp"))
        .file(&glue_ym2610)
        .file(&glue_ym2151)
        .include(&ymfm_src)
        .opt_level(3)
        .flag_if_supported("-fno-strict-aliasing")
        .flag_if_supported("-fomit-frame-pointer")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-array-bounds")
        .compile("ym2610_ymfm");

    println!("cargo:rustc-link-lib=c++");

    // SDL2 link-search helper for the optional emufight-sdl binary.
    if std::env::var("CARGO_FEATURE_SDL_HOST").is_ok() && !target.contains("wasm32") {
        println!("cargo:rerun-if-env-changed=SDL2_PATH");
        println!("cargo:rerun-if-env-changed=HOMEBREW_PREFIX");

        if let Ok(p) = std::env::var("SDL2_PATH") {
            println!("cargo:rustc-link-search=native={p}/lib");
            println!("cargo:rustc-link-search=native={p}");
        } else {
            let mut candidates = vec!["/opt/homebrew".into(), "/usr/local".into()];
            if let Ok(p) = std::env::var("HOMEBREW_PREFIX") {
                candidates.insert(0, p);
            }
            if let Ok(out) = std::process::Command::new("brew").args(["--prefix", "sdl2"]).output() {
                if out.status.success() {
                    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !p.is_empty() {
                        candidates.insert(0, p);
                    }
                }
            }
            for prefix in candidates {
                for sub in ["lib", "lib64"] {
                    let lib = format!("{prefix}/{sub}");
                    let marker_a = format!("{lib}/libSDL2.dylib");
                    let marker_b = format!("{lib}/libSDL2-2.0.0.dylib");
                    let marker_c = format!("{lib}/libSDL2.so");
                    if std::path::Path::new(&marker_a).is_file()
                        || std::path::Path::new(&marker_b).is_file()
                        || std::path::Path::new(&marker_c).is_file()
                    {
                        println!("cargo:rustc-link-search=native={lib}");
                        return;
                    }
                }
            }
        }
    }
}
