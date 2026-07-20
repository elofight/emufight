//! Netplay **boot savestates** (character-select / match-ready), not ROMs.
//!
//! # Policy
//!
//! - **Never** ship game or system ROM dumps in this crate.
//! - **Do** ship (or capture) small `charselect.bin` blobs so online peers share
//!   the same frame-0 when possible.
//!
//! # Layout (cwd-relative)
//!
//! ```text
//! boot/<game_id>/charselect.bin   # preferred (this repo)
//! data/<game_id>/charselect.bin   # host / product fallback
//! roms/<game_id>/charselect.bin   # optional host-side fallback
//! ```
//!
//! Capture (requires *your* licensed ROM set on disk):
//!
//! ```sh
//! cargo run -p emufight-sdl --bin emufight-capture-boot -- kof98
//! cargo run -p emufight-sdl --bin emufight-capture-boot -- sf2ce
//! ```

use std::path::{Path, PathBuf};

/// File name used for the default netplay boot savestate.
pub const CHARSELECT_BIN: &str = "charselect.bin";

/// Preferred directory name for boot artifacts (never ROM sets).
pub const BOOT_DIR: &str = "boot";

/// Candidate paths for a game's boot savestate, highest priority first.
pub fn initial_match_state_paths(game: &str) -> Vec<PathBuf> {
    [
        Path::new(BOOT_DIR).join(game).join(CHARSELECT_BIN),
        Path::new("data").join(game).join(CHARSELECT_BIN),
        Path::new("roms").join(game).join(CHARSELECT_BIN),
    ]
    .into_iter()
    .collect()
}

/// Default capture output path: `boot/<game>/charselect.bin`.
pub fn default_capture_path(game: &str) -> PathBuf {
    Path::new(BOOT_DIR).join(game).join(CHARSELECT_BIN)
}
