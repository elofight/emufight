use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Paths are relative to this crate's directory.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
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
    let is_wasm = target.contains("wasm32");

    if is_wasm {
        compile_ymfm_wasm(&ymfm_src, &glue_ym2610, &glue_ym2151);
        return;
    }

    compile_ymfm_native(&target, &ymfm_src, &glue_ym2610, &glue_ym2151);
    link_sdl2_search_if_needed(&target);
}

/// Desktop / native: standard `cc` + system C++ runtime.
fn compile_ymfm_native(
    target: &str,
    ymfm_src: &Path,
    glue_ym2610: &Path,
    glue_ym2151: &Path,
) {
    let is_msvc = target.contains("msvc");
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17");
    // PIC is a Unix concept; on MSVC it can confuse flags.
    if !is_msvc {
        build.pic(true);
    }

    if target.contains("apple") {
        if let Ok(output) = Command::new("xcrun").arg("--show-sdk-path").output() {
            let sdk_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !sdk_path.is_empty() {
                build.include(format!("{}/usr/include/c++/v1", sdk_path));
            }
        }
    }

    if is_msvc {
        // MSVC: exceptions for any C++ that needs them; quiet noisy headers.
        build.flag("/EHsc");
        build.flag("/wd4100"); // unreferenced formal parameter
        build.flag("/wd4189"); // local variable initialized but not referenced
        build.flag("/wd4244"); // conversion warnings in ymfm
        build.flag("/wd4267");
        build.flag("/wd4996"); // getenv etc.
    }

    build
        .file(ymfm_src.join("ymfm_adpcm.cpp"))
        .file(ymfm_src.join("ymfm_ssg.cpp"))
        .file(ymfm_src.join("ymfm_opn.cpp"))
        .file(ymfm_src.join("ymfm_opm.cpp"))
        .file(ymfm_src.join("ymfm_pcm.cpp"))
        .file(glue_ym2610)
        .file(glue_ym2151)
        .include(ymfm_src)
        .opt_level(3)
        .flag_if_supported("-fno-strict-aliasing")
        .flag_if_supported("-fomit-frame-pointer")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-array-bounds")
        .compile("ym2610_ymfm");

    // MSVC links the C++ CRT via the objects themselves; do not request `-lc++`.
    if !is_msvc {
        if target.contains("apple") || target.contains("linux") || target.contains("freebsd") {
            println!("cargo:rustc-link-lib=c++");
        }
    }
}

/// WASM: compile ymfm with Emscripten (`emcc`/`emar`).
///
/// `cc-rs` injects `--target=wasm32-unknown-unknown`, which emcc rejects.
/// We invoke emcc ourselves targeting `wasm32-unknown-emscripten` (produces
/// wasm objects that rustc still links into `wasm32-unknown-unknown`).
/// Requires `emcc` on PATH (Homebrew: `emscripten`).
fn compile_ymfm_wasm(ymfm_src: &Path, glue_ym2610: &Path, glue_ym2151: &Path) {
    let emcc = which("emcc").unwrap_or_else(|| {
        panic!(
            "emcc not found on PATH — install Emscripten to build NeoGeo/CPS sound for WASM \
             (e.g. `brew install emscripten`)."
        )
    });
    let emar = which("emar").unwrap_or_else(|| "emar".into());

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let opt = if std::env::var("PROFILE").as_deref() == Ok("release") {
        "-O3"
    } else {
        "-O2"
    };

    let sources: Vec<PathBuf> = vec![
        ymfm_src.join("ymfm_adpcm.cpp"),
        ymfm_src.join("ymfm_ssg.cpp"),
        ymfm_src.join("ymfm_opn.cpp"),
        ymfm_src.join("ymfm_opm.cpp"),
        ymfm_src.join("ymfm_pcm.cpp"),
        glue_ym2610.to_path_buf(),
        glue_ym2151.to_path_buf(),
    ];

    let mut objects = Vec::new();
    for src in &sources {
        let stem = src.file_stem().unwrap().to_string_lossy();
        let obj = out_dir.join(format!("{stem}.o"));
        println!("cargo:rerun-if-changed={}", src.display());

        let status = Command::new(&emcc)
            .args([
                "-c",
                opt,
                "-std=c++17",
                "-fno-exceptions",
                "-fno-rtti",
                "-fno-threadsafe-statics",
                "-ffunction-sections",
                "-fdata-sections",
                // emcc defaults to wasm32-unknown-emscripten (not rust's triple).
                "-sSTRICT=0",
            ])
            .arg(format!("-I{}", ymfm_src.display()))
            .arg(src)
            .arg("-o")
            .arg(&obj)
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn emcc: {e}"));

        if !status.success() {
            panic!(
                "emcc failed compiling {} (status {status}). Is emscripten installed?",
                src.display()
            );
        }
        objects.push(obj);
    }

    let lib = out_dir.join("libym2610_ymfm.a");
    // emar rcs lib.a a.o b.o ...
    let status = Command::new(&emar)
        .arg("rcs")
        .arg(&lib)
        .args(&objects)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn emar: {e}"));
    if !status.success() {
        panic!("emar failed creating {}", lib.display());
    }

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=ym2610_ymfm");
}

fn link_sdl2_search_if_needed(target: &str) {
    if std::env::var("CARGO_FEATURE_SDL_HOST").is_err() || target.contains("wasm32") {
        return;
    }
    println!("cargo:rerun-if-env-changed=SDL2_PATH");
    println!("cargo:rerun-if-env-changed=HOMEBREW_PREFIX");

    if let Ok(p) = std::env::var("SDL2_PATH") {
        println!("cargo:rustc-link-search=native={p}/lib");
        println!("cargo:rustc-link-search=native={p}");
        return;
    }

    let mut candidates = vec!["/opt/homebrew".into(), "/usr/local".into()];
    if let Ok(p) = std::env::var("HOMEBREW_PREFIX") {
        candidates.insert(0, p);
    }
    if let Ok(out) = Command::new("brew").args(["--prefix", "sdl2"]).output() {
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
            if Path::new(&marker_a).is_file()
                || Path::new(&marker_b).is_file()
                || Path::new(&marker_c).is_file()
            {
                println!("cargo:rustc-link-search=native={lib}");
                return;
            }
        }
    }
}

fn which(bin: &str) -> Option<String> {
    if let Ok(p) = std::env::var(bin.to_ascii_uppercase()) {
        if Path::new(&p).is_file() {
            return Some(p);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path.split(sep) {
            let candidate = Path::new(dir).join(bin);
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
            // Windows: emcc.bat / emcc.ps1 via emsdk
            if cfg!(windows) {
                for ext in [".exe", ".bat", ".cmd"] {
                    let c = Path::new(dir).join(format!("{bin}{ext}"));
                    if c.is_file() {
                        return Some(c.display().to_string());
                    }
                }
            }
        }
    }
    None
}
