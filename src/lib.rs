//! **emufight** — frame-driven arcade cores with deterministic save-states
//! and optional GGRS rollback netplay.
//!
//! Hosts supply ROM data, map controls to [`InputState`], present
//! framebuffer/audio, and (for online play) implement
//! `ggrs::NonBlockingSocket` — or use `netplay::SimSocket` for tests.
//!
//! # Quick start
//!
//! ```no_run
//! use emufight::{create_emulator_for_platform, InputState};
//!
//! use emufight::RomSource;
//! let mut emu = create_emulator_for_platform("neogeo").unwrap();
//! // Host places dumps under roms/<name>/ (and system ROMs under data/neogeo/ or roms/neogeo/).
//! emu.load_roms(RomSource::disk("kof98")).unwrap();
//! emu.reset();
//!
//! loop {
//!     emu.set_input(InputState::default());
//!     let frame = emu.step(735); // 44 100 Hz / 60 fps
//!     // frame.framebuffer — RGB24
//!     // frame.audio       — f32 mono 44.1 kHz
//! }
//! ```
//!
//! # Save states / rollback
//!
//! ```no_run
//! # use emufight::Emulator;
//! # let mut emu = Emulator::new();
//! let blob: Vec<u8> = emu.save_state_to_bytes().unwrap();
//! emu.load_state_from_bytes(&blob).unwrap();
//! ```

pub mod io;
pub mod neogeo;
pub mod core;
pub mod cps;
pub mod catalog;
pub mod boot;

/// Disk / zip ROM-set helpers. Download helpers need `native-romset`.
///
/// **ROM images are never shipped with this crate** — host supplies dumps.
pub mod romset;

#[cfg(feature = "netplay")]
pub mod netplay;

pub mod save_state;
pub mod replay;
pub mod trace;

pub use io::InputState;
pub use core::{EmulatorCore, FrameOutput, RomSource};
pub use neogeo::Emulator;
pub use cps::CpsEmulator;
pub use save_state::SaveState;
pub use replay::InputDriver;
pub use catalog::RomCatalog;
pub use boot::{default_capture_path, initial_match_state_paths, CHARSELECT_BIN};

/// Instantiate a core for an explicit platform id from the host catalog.
///
/// | `platform` | Core |
/// |---|---|
/// | `"neogeo"`, `"neo"`, `"mvs"`, `"aes"` | NeoGeo [`Emulator`] |
/// | `"cps1"`, `"cps"` | [`CpsEmulator`] |
///
/// Unknown ids return `Err` (they do **not** silently default).
pub fn create_emulator_for_platform(platform: &str) -> Result<Box<dyn EmulatorCore>, String> {
    match platform.to_ascii_lowercase().as_str() {
        "cps1" | "cps" => Ok(Box::new(CpsEmulator::new())),
        "neogeo" | "neo" | "mvs" | "aes" => Ok(Box::new(Emulator::new())),
        other => Err(format!(
            "unknown platform '{other}': expected \"neogeo\" or \"cps1\""
        )),
    }
}

/// Instantiate a core using the host-supplied [`RomCatalog`] for platform dispatch.
///
/// When `name` is missing from the catalog (or has no `platform` field), NeoGeo
/// is assumed.
pub fn create_emulator(name: &str, catalog: &RomCatalog) -> Result<Box<dyn EmulatorCore>, String> {
    let platform = catalog.platform_for(name).unwrap_or("neogeo");
    create_emulator_for_platform(platform)
}

/// Nominal audio output rate (Hz).
pub const AUDIO_SAMPLE_RATE: u32 = 44_100;

/// Nominal samples per video frame at 60 fps.
pub const NOMINAL_SAMPLES_PER_FRAME: usize = AUDIO_SAMPLE_RATE as usize / 60; // 735

/// C/C++ runtime shims so Emscripten-built ymfm links into wasm-bindgen.
/// Side-effect only (`#[no_mangle]`); nothing imports this module by name.
#[cfg(target_arch = "wasm32")]
mod wasm_stubs;

/// Read every file under `dir` into a basename→bytes map (for [`RomSource::Files`]).
pub fn files_from_dir(dir: &std::path::Path) -> Result<std::collections::HashMap<String, Vec<u8>>, String> {
    let mut files = std::collections::HashMap::new();
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let data = std::fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
                files.insert(name.to_string(), data);
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use std::path::PathBuf;

    /// Product cores must load from a file map (WASM contract). Disk is optional
    /// when roms/ is missing; Files path must not return "not supported".
    #[test]
    fn product_cores_implement_rom_source_files() {
        // Empty map: must fail with a content error, never "not supported".
        let empty = std::collections::HashMap::new();
        for (platform, game) in [("neogeo", "kof98"), ("cps1", "sf2ce")] {
            let mut emu = create_emulator_for_platform(platform).unwrap();
            let err = emu
                .load_roms(RomSource::files(game, &empty))
                .expect_err("empty map should fail");
            assert!(
                !err.to_ascii_lowercase().contains("not supported"),
                "{platform}: unexpected not-supported: {err}"
            );
        }

        // When dumps exist, Files must succeed for both product games.
        for (platform, game) in [("neogeo", "kof98"), ("cps1", "sf2ce")] {
            let dir = PathBuf::from(format!("roms/{game}"));
            if !dir.is_dir() {
                eprintln!("skip Files success for {game}: no {dir:?}");
                continue;
            }
            let files = files_from_dir(&dir).expect("read roms");
            let mut emu = create_emulator_for_platform(platform).unwrap();
            emu.load_roms(RomSource::files(game, &files))
                .unwrap_or_else(|e| panic!("{platform}/{game} Files load: {e}"));
        }
    }
}
