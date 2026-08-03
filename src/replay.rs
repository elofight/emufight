//! Replay and recording infrastructure.
//!
//! # Overview
//!
//! The replay system works at the *frame* granularity: each frame's worth of
//! player input is stored as a `FrameInput` record.  This is coarse enough to
//! be compact and portable, yet fine enough to reproduce any game sequence
//! deterministically given a matching save state at the starting frame.
//!
//! ## Modes
//!
//! ```text
//! InputDriver::Live              — normal play; input from hardware each frame
//! InputDriver::Record(session)   — normal play + records inputs to a file
//! InputDriver::Replay(session)   — drives input from a previously recorded file
//! ```
//!
//! ## File format
//!
//! Replay files are JSON arrays of `FrameInput` objects (`.nrp` — NeoGeo
//! Replay).  JSON is chosen for human readability and debuggability; a typical
//! 10-second replay at 60 fps is ~50 KB uncompressed, which is trivial.
//!
//! ```json
//! [
//!   {"frame":0,  "p1":255,"p2":255,"sys":255,"coin":255},
//!   {"frame":37, "p1":253,"p2":255,"sys":255,"coin":255},
//!   ...
//! ]
//! ```
//!
//! Only frames where input *changes* relative to the previous frame need to be
//! recorded, but the loader accepts any subset and fills gaps with the last
//! seen state.
//!
//! ## Usage
//!
//! ```ignore
//! // Recording
//! let mut driver = InputDriver::new_record("replay.nrp");
//! // In the main loop:
//! let input = driver.next_frame(frame_number, live_input);
//! // On exit:
//! driver.save_if_recording()?;
//!
//! // Replay
//! let driver = InputDriver::new_replay("replay.nrp")?;
//! let input = driver.next_frame(frame_number, live_input); // live_input ignored
//! ```

use crate::io::{InputState, PackedInput, pack_input, unpack_input};
use std::io::{Read, Write};
use flate2::write::ZlibEncoder;
use flate2::read::ZlibDecoder;
use flate2::Compression;

// ── ReplaySession ─────────────────────────────────────────────────────────────

pub struct ReplaySession {
    frames: Vec<(PackedInput, PackedInput)>,
    cursor: usize,
}

impl ReplaySession {
    pub fn load_from_file(path: &str) -> Result<Self, String> {
        let compressed = std::fs::read(path)
            .map_err(|e| format!("replay read '{}': {}", path, e))?;
            
        let mut raw_bytes = Vec::new();
        if compressed.len() >= 2 && compressed[0] == 0x78 && (compressed[1] == 0x01 || compressed[1] == 0x9c || compressed[1] == 0xda) {
            let mut decoder = ZlibDecoder::new(&compressed[..]);
            decoder.read_to_end(&mut raw_bytes)
                .map_err(|e| format!("replay decompress: {}", e))?;
        } else {
            raw_bytes = compressed;
        }
            
        let u32_slice: &[u32] = bytemuck::try_cast_slice(&raw_bytes)
            .map_err(|e| format!("replay bytemuck cast: {}", e))?;
            
        let mut frames = Vec::with_capacity(u32_slice.len() / 2);
        for chunk in u32_slice.chunks_exact(2) {
            frames.push((chunk[0], chunk[1]));
        }
        
        log::info!("Replay loaded: {} ({} frames)", path, frames.len());
        Ok(ReplaySession { frames, cursor: 0 })
    }

    pub fn next_frame(&mut self, _frame: u64) -> InputState {
        if self.frames.is_empty() {
            return InputState::default();
        }
        let idx = self.cursor.min(self.frames.len().saturating_sub(1));
        let (p0_packed, p1_packed) = self.frames[idx];
        self.cursor += 1;
        
        let p0 = unpack_input(p0_packed);
        let p1 = unpack_input(p1_packed);
        
        let mut combined_sys = p0.sys;
        if (p1.sys & 0x01) == 0 { combined_sys &= !0x04u8; } // P2 Start
        
        let mut combined_coin = p0.coin;
        if (p1.coin & 0x01) == 0 { combined_coin &= !0x02u8; }

        InputState {
            p1: p0.p1,
            p2: p1.p1,
            sys: combined_sys,
            coin: combined_coin,
            ext: crate::io::combine_ext(p0.ext, p1.ext),
        }
    }

    pub fn is_done(&self, _frame: u64) -> bool {
        self.cursor >= self.frames.len()
    }
}

// ── RecordSession ─────────────────────────────────────────────────────────────

pub struct RecordSession {
    frames: Vec<(PackedInput, PackedInput)>,
    path:   String,
}

impl RecordSession {
    pub fn new(path: &str) -> Self {
        RecordSession {
            frames: Vec::new(),
            path:   path.to_owned(),
        }
    }

    pub fn record(&mut self, _frame: u64, input: &InputState) {
        // Split the combined input into two per-peer packed values.  Each peer
        // carries its own E/F in ext bits 0–1: P1's E/F are combined ext bits
        // 0–1, P2's are bits 2–3.  Upper bits stay released (active-low).
        let p0_inp = InputState {
            p1: input.p1,
            p2: 0xFF,
            sys: input.sys,
            coin: input.coin,
            ext: (input.ext & 0x03) | 0x0C,
        };
        let p1_inp = InputState {
            p1: input.p2,
            p2: 0xFF,
            sys: 0xFF,
            coin: 0xFF,
            ext: ((input.ext >> 2) & 0x03) | 0x0C,
        };
        self.frames.push((pack_input(&p0_inp), pack_input(&p1_inp)));
    }

    pub fn save(&self) -> Result<(), String> {
        let mut flat = Vec::with_capacity(self.frames.len() * 2);
        for &(p0, p1) in &self.frames {
            flat.push(p0);
            flat.push(p1);
        }
        let raw_bytes = bytemuck::cast_slice(&flat);
        
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(raw_bytes)
            .map_err(|e| format!("replay compress: {}", e))?;
        let compressed = encoder.finish()
            .map_err(|e| format!("replay compress finish: {}", e))?;
            
        std::fs::write(&self.path, compressed)
            .map_err(|e| format!("replay write '{}': {}", self.path, e))?;
        log::info!("Replay saved: {} ({} frames)", self.path, self.frames.len());
        Ok(())
    }
}

// ── InputDriver ───────────────────────────────────────────────────────────────

pub enum InputDriver {
    Live,
    Replay(ReplaySession),
    Record(RecordSession),
}

impl InputDriver {
    pub fn live() -> Self { InputDriver::Live }

    pub fn new_record(path: &str) -> Self {
        InputDriver::Record(RecordSession::new(path))
    }

    pub fn new_replay(path: &str) -> Result<Self, String> {
        Ok(InputDriver::Replay(ReplaySession::load_from_file(path)?))
    }

    pub fn next_frame(&mut self, frame: u64, live: InputState) -> InputState {
        match self {
            InputDriver::Live => live,
            InputDriver::Replay(s) => s.next_frame(frame),
            InputDriver::Record(s) => {
                s.record(frame, &live);
                live
            }
        }
    }

    pub fn save_if_recording(&self) -> Result<(), String> {
        match self {
            InputDriver::Record(s) => s.save(),
            _ => Ok(()),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_input(p1: u8) -> InputState {
        InputState { p1, p2: 0xFF, sys: 0xFF, coin: 0xFF, ext: 0x0F }
    }

    #[test]
    fn live_driver_passes_through_input() {
        let mut driver = InputDriver::live();
        let live = make_input(0xFE); // button A held
        let out = driver.next_frame(0, live.clone());
        assert_eq!(out.p1, 0xFE);
    }

    #[test]
    fn record_stores_and_replay_reproduces() {
        use std::fs;
        let path = "/tmp/neo_test_replay.nrb";

        {
            let mut driver = InputDriver::new_record(path);
            driver.next_frame(0, make_input(0xFF)); // idle
            driver.next_frame(1, make_input(0xFE)); // A pressed
            driver.next_frame(2, make_input(0xFD)); // B pressed
            driver.save_if_recording().unwrap();
        }

        {
            let mut driver = InputDriver::new_replay(path).unwrap();
            let f0 = driver.next_frame(0, make_input(0x00));
            let f1 = driver.next_frame(1, make_input(0x00));
            let f2 = driver.next_frame(2, make_input(0x00));
            assert_eq!(f0.p1, 0xFF, "frame 0: idle");
            assert_eq!(f1.p1, 0xFE, "frame 1: A pressed");
            assert_eq!(f2.p1, 0xFD, "frame 2: B pressed");
        }

        fs::remove_file(path).ok();
    }
}
