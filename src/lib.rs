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
//! let mut emu = create_emulator_for_platform("neogeo").unwrap();
//! // Host places dumps under roms/<name>/ (and system ROMs under data/neogeo/ or roms/neogeo/).
//! emu.load_roms(Some("kof98")).unwrap();
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
pub use core::{EmulatorCore, FrameOutput};
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
pub mod wasm_stubs;
