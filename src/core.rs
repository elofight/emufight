//! Common emulator-core trait — any system (NeoGeo, CPS-2, …) implements this.
//!
//! The [`EmulatorCore`] trait provides a frame-level API that host applications
//! (desktop, browser, headless) can drive without knowing the underlying hardware.
//! Optional I/O traits live in `crate::io` (`VideoSink`, `AudioSink`, `InputSource`).
//!
//! # Adding a new system
//!
//! 1. Implement [`EmulatorCore`] for the new board.
//! 2. Register a platform id in the host's [`crate::RomCatalog`] / factory.
//! 3. Netplay, replay, and save-state plumbing work unchanged via the trait.

use crate::io::InputState;

// ── Save-state identity header ────────────────────────────────────────────────

/// Magic prefix stamped on binary save states so a blob produced by one core
/// cannot be silently deserialised into another (which would corrupt state).
pub const SAVE_MAGIC: [u8; 4] = *b"NEOS";

/// Core identifier for NeoGeo save states.
pub const SAVE_CORE_NEOGEO: u8 = 1;

/// Core identifier for CPS-1 save states.
pub const SAVE_CORE_CPS1: u8 = 2;

/// Prepend the identity header (`SAVE_MAGIC` + `core_id`) to a serialised body.
pub fn with_save_header(core_id: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + SAVE_MAGIC.len() + 1);
    out.extend_from_slice(&SAVE_MAGIC);
    out.push(core_id);
    out.extend_from_slice(&body);
    out
}

/// Validate and strip the identity header, returning the serialised body.
///
/// When the header is present it must match `expected_core`, otherwise a
/// descriptive error is returned instead of letting the wrong core attempt a
/// deserialise.  Legacy blobs without a header are accepted unchanged for
/// backward compatibility with older host-supplied boot states.
pub fn strip_save_header(data: &[u8], expected_core: u8) -> Result<&[u8], String> {
    if data.len() >= SAVE_MAGIC.len() + 1 && data[..SAVE_MAGIC.len()] == SAVE_MAGIC {
        let core = data[SAVE_MAGIC.len()];
        if core != expected_core {
            return Err(format!(
                "save state core mismatch: blob is for core {}, this emulator is core {}",
                core, expected_core
            ));
        }
        Ok(&data[SAVE_MAGIC.len() + 1..])
    } else {
        // Legacy headerless blob — accept for backward compatibility.
        Ok(data)
    }
}

/// Output produced by `EmulatorCore::step`.
///
/// Borrows directly into the emulator's internal buffers — no copy is made.
pub struct FrameOutput<'a> {
    /// RGB24 pixel data, system-dependent resolution, row-major.
    pub framebuffer: &'a [u8],
    /// Audio samples (mono f32) at the system's sample rate.
    pub audio: &'a [f32],
}

/// Frame-level emulator interface, decoupled from any specific system.
///
/// Every method has a default where meaningful — override the ones your
/// system supports.  Machine-specific diagnostics (`debug_pc`, …) return
/// sensible defaults when unused.
pub trait EmulatorCore: Send {
    /// Load ROMs for a named game (looked up in the system's manifest).
    fn load_roms(&mut self, name: Option<&str>) -> Result<(), String>;

    /// Cold-reset the system.
    fn reset(&mut self);

    /// Latch input for the upcoming frame.
    fn set_input(&mut self, state: InputState);

    /// Advance one frame, returning the rendered framebuffer and audio.
    fn step(&mut self, n_audio_samples: usize) -> FrameOutput<'_>;

    /// Borrow the last frame's RGB24 framebuffer.
    fn framebuffer(&self) -> &[u8];

    /// Borrow the last frame's audio samples.
    fn audio_samples(&self) -> &[f32];

    /// Monotonic frame counter.
    fn frame(&self) -> u64;

    /// Fast headless step — runs CPU + video logic but skips framebuffer
    /// composition and audio generation.  Used during catch-up where only
    /// deterministic state progression matters.
    fn step_cpu(&mut self);

    /// Borrow the system work RAM (NeoGeo-specific; returns empty slice
    /// for non-NeoGeo cores).
    fn work_ram(&self) -> &[u8] { &[] }

    // ── Save states ────────────────────────────────────────────────────────────

    /// Serialise all mutable state to a binary blob (for rollback / save files).
    fn save_state_to_bytes(&mut self) -> Result<Vec<u8>, String>;

    /// Returns 8 packed 16-bit FNV-1a checksums for rollback desync debugging.
    fn state_debug_checksums(&self) -> Option<[u16; 8]> { None }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> { None }

    /// Captures the emulator state once and returns both the binary blob and the debug checksums.
    fn save_state_and_checksums(&mut self) -> Result<(Vec<u8>, Option<[u16; 8]>), String> {
        Ok((self.save_state_to_bytes()?, self.state_debug_checksums()))
    }

    /// Restore state from a blob produced by `save_state_to_bytes`.
    fn load_state_from_bytes(&mut self, data: &[u8]) -> Result<(), String>;

    /// Write a human-readable save state to a JSON file.
    fn save_state_to_file(&self, path: &str) -> Result<(), String>;

    /// Load a save state from a JSON file.
    fn load_state_from_file(&mut self, path: &str) -> Result<(), String>;

    /// Flush any in-progress recording to disk.
    fn save_recording(&mut self) -> Result<(), String> { Ok(()) }

    // ── Persistent storage ─────────────────────────────────────────────────────

    /// Borrow battery-backed SRAM (system-specific size).
    fn backup_ram(&self) -> &[u8];

    /// Load battery-backed SRAM from a byte slice.
    fn load_sram(&mut self, data: &[u8]) -> bool;

    // ── System info ────────────────────────────────────────────────────────────

    /// Active display resolution in pixels `(width, height)`.
    fn resolution(&self) -> (u32, u32);

    /// Audio output sample rate in Hz.
    fn audio_sample_rate(&self) -> u32;

    /// Native vertical refresh rate in Hz used for wall-clock frame pacing.
    ///
    /// This is the emulated hardware's true field rate (independent of the
    /// host monitor or CPU speed) so the shell can throttle emulation to the
    /// correct speed regardless of vsync rate or build profile.  Defaults to
    /// 60 Hz; systems override with their exact rate.
    fn refresh_rate(&self) -> f64 { 60.0 }

    // ── Diagnostics / machine-specific (override per system) ──────────────────

    /// Current program counter of the main CPU (for debug logging).
    fn debug_pc(&self) -> u32 { 0 }

    /// Total video frames rendered.
    fn video_frame_count(&self) -> usize { 0 }

    /// Total audio samples produced.
    fn audio_sample_count(&self) -> u64 { 0 }

    /// Enable the system's operator / service menu (if any).
    fn enable_operator_menu(&mut self) {}

    /// Present as AES/home (REG_STATUS_B bit 7 low). NeoGeo only; no-op elsewhere.
    fn enable_aes_home(&mut self) {}

    /// Present as MVS/arcade (REG_STATUS_B bit 7 high). NeoGeo only; no-op elsewhere.
    fn enable_mvs_presentation(&mut self) {}

    /// Load a netplay boot savestate (`boot/<game>/charselect.bin`, etc.).
    ///
    /// **Not a ROM** — a captured machine snapshot so peers share frame-0.
    /// ROM dumps are never compiled into this library. Returns `true` on success.
    fn load_initial_match_state(&mut self) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_header_roundtrips_same_core() {
        let framed = with_save_header(SAVE_CORE_NEOGEO, vec![1, 2, 3, 4]);
        assert_eq!(&framed[..4], &SAVE_MAGIC);
        assert_eq!(framed[4], SAVE_CORE_NEOGEO);
        let body = strip_save_header(&framed, SAVE_CORE_NEOGEO).unwrap();
        assert_eq!(body, &[1, 2, 3, 4]);
    }

    #[test]
    fn save_header_rejects_wrong_core() {
        let neogeo_blob = with_save_header(SAVE_CORE_NEOGEO, vec![9, 9, 9]);
        // Loading a NeoGeo blob as CPS-1 (or vice versa) must be a clean error,
        // not a silent corrupt deserialise.
        assert!(strip_save_header(&neogeo_blob, SAVE_CORE_CPS1).is_err());
    }

    #[test]
    fn save_header_accepts_legacy_headerless() {
        // Blobs baked before headers existed have no magic prefix and must
        // still load unchanged.
        let legacy = vec![0x07, 0x00, 0x00, 0x00, 0xAB];
        let body = strip_save_header(&legacy, SAVE_CORE_NEOGEO).unwrap();
        assert_eq!(body, legacy.as_slice());
    }
}
