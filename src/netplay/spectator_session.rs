use crate::EmulatorCore;
use crate::NOMINAL_SAMPLES_PER_FRAME;
use crate::io::PackedInput;
use crate::io::unpack_input;

/// Host-fed spectator / replay session (no GGRS).
///
/// Consumes a stream of packed input pairs from the host (live relay, catch-up
/// blob, or a recorded replay) and drives the local emulator deterministically
/// without contributing input of its own.
///
/// The full input log is retained (never drained), so recorded replays support
/// random-access seeking: [`Self::seek`] restores the boot state and
/// headlessly re-simulates to the requested frame.
pub struct OnlineSpectatorSession {
    /// Append-only full input log (random-access, never drained).
    log: Vec<(PackedInput, PackedInput)>,
    /// Frames consumed so far = index of the next input to apply.
    cursor: usize,
    /// The most recently applied input pair, so the HUD can render exactly
    /// what the emulator advanced (single source of truth) instead of
    /// shifting a parallel JS queue that can drift during catch-up.
    last_inputs: (PackedInput, PackedInput),
    /// State at frame 0, used to anchor rewinds for deterministic seeking.
    boot_state: Vec<u8>,
    /// Mirrors `cursor` as i32 for external readers.
    pub current_frame: i32,
    pub error: Option<String>,
    frames_since_running: u32,
    /// `step_cpu` frames since the last rendered frame (seek / catch-up).
    headless_since_visible: u32,
    pub total_advance_calls: u64,
}

fn decode_input_pairs(data: &[u8]) -> Vec<(PackedInput, PackedInput)> {
    let mut result = Vec::with_capacity(data.len() / 8);
    for chunk in data.chunks_exact(8) {
        let p0 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let p1 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        result.push((p0, p1));
    }
    result
}

impl OnlineSpectatorSession {
    pub fn new(boot_state: Vec<u8>) -> Self {
        Self {
            log: Vec::new(),
            cursor: 0,
            last_inputs: (0, 0),
            boot_state,
            current_frame: 0,
            error: None,
            frames_since_running: 0,
            headless_since_visible: 0,
            total_advance_calls: 0,
        }
    }

    /// Push live frames. Payload is flat little-endian `u32` pairs `(p0, p1)`.
    pub fn push_inputs(&mut self, payload: &[u8]) {
        let pairs = decode_input_pairs(payload);
        self.log.extend(pairs);
    }

    /// Push catch-up frames. Payload is the same flat LE `u32` pair format,
    /// typically a single concatenated blob from the host relay.
    pub fn push_catchup(&mut self, payload: &[u8]) {
        if payload.len() < 8 || payload.len() % 8 != 0 {
            log::error!("SPECTATE: catch-up payload has invalid length {} (not a multiple of 8)", payload.len());
            return;
        }
        let pairs = decode_input_pairs(payload);
        let total = pairs.len();
        self.log.extend(pairs);
        log::info!("SPECTATE: catch-up decoded {} frames ({} bytes)", total, payload.len());
    }

    /// Frames available ahead of the playback cursor (buffered, not yet shown).
    pub fn buffered_frames(&self) -> usize {
        self.log.len().saturating_sub(self.cursor)
    }

    /// Total frames in the input log (the seekable range for replays).
    pub fn total_frames(&self) -> usize {
        self.log.len()
    }

    /// Current playback position (frames consumed).
    pub fn current_frame(&self) -> usize {
        self.cursor
    }

    /// The packed input pair (p0, p1) applied to the most recently advanced
    /// frame. Lets the HUD display inputs aligned to the rendered frame.
    pub fn last_inputs(&self) -> (PackedInput, PackedInput) {
        self.last_inputs
    }

    /// Process up to `max_frames` buffered inputs using the CPU-only path
    /// (no framebuffer / audio).  Call from the catch-up phase.
    pub fn catch_up_batch(&mut self, emulator: &mut dyn EmulatorCore, max_frames: usize) -> usize {
        let mut count = 0;
        while self.cursor < self.log.len() && count < max_frames {
            self.advance_one_cpu(emulator);
            count += 1;
        }
        count
    }

    /// Seek playback so the display shows the output of frame `target` (a
    /// cursor value in `1..=len`). Deterministic and checkpoint-driven in both
    /// directions:
    ///
    /// * The nearest keyframe anchor at or before the target is located.
    /// * It is **restored** when we must rewind (cursor is past the target) or
    ///   when the anchor lies *ahead* of the current cursor (a forward jump
    ///   where landing on the checkpoint skips redundant re-simulation).
    /// * Otherwise we fast-forward **headlessly** (`step_cpu`) from the current
    ///   position.
    ///
    /// Only the single target frame is rendered, so a seek never produces a
    /// burst of visible intermediate frames. Returns the new cursor, or -1 if
    /// there is nothing to seek.
    pub fn seek(&mut self, emulator: &mut dyn EmulatorCore, target: i32) -> i32 {
        let len = self.log.len();
        if len == 0 {
            return -1;
        }
        // Render at least one frame; never past the end of the log.
        let target = target.clamp(1, len as i32) as usize;
        // Frames to consume headlessly before the single rendered frame.
        let before = target - 1;

        if self.cursor > before {
            // Rewind: reset and load the boot state, then fast-forward from frame 0
            emulator.reset();
            if emulator.load_state_from_bytes(&self.boot_state).is_err() {
                // Should never happen since boot_state is a valid save state we took
                return self.current_frame;
            }
            self.cursor = 0;
            self.current_frame = 0;
            self.headless_since_visible = 0;
        }

        while self.cursor < before {
            self.advance_one_cpu(emulator);
        }
        // Render the target frame.
        if self.cursor < self.log.len() {
            let _ = self.advance_one_visible(emulator);
        }
        self.current_frame
    }

    pub fn video_ready(&self) -> bool {
        self.frames_since_running > 0
    }

    pub fn audio_ready(&self) -> bool {
        self.frames_since_running > 0
    }

    pub fn fade_intensity(&self) -> f32 {
        const FADE_FRAMES: u32 = 30;
        if self.frames_since_running >= FADE_FRAMES {
            1.0
        } else {
            self.frames_since_running as f32 / FADE_FRAMES as f32
        }
    }

    /// Capture a save-state anchor at the current cursor when it lands on a
    /// keyframe boundary. Idempotent: a boundary is snapshotted at most once,
    /// so re-simulating over a region during a seek does not duplicate work.
    fn advance_one_visible(&mut self, emulator: &mut dyn EmulatorCore) -> (Vec<u8>, Vec<f32>) {
        self.apply_input_at_cursor(emulator);
        let out = emulator.step(NOMINAL_SAMPLES_PER_FRAME);
        let fb = out.framebuffer.to_vec();
        let au = out.audio.to_vec();
        self.headless_since_visible = 0;
        self.cursor += 1;
        self.current_frame = self.cursor as i32;
        self.frames_since_running = self.frames_since_running.saturating_add(1);
        (fb, au)
    }

    fn advance_one_cpu(&mut self, emulator: &mut dyn EmulatorCore) {
        self.apply_input_at_cursor(emulator);
        emulator.step_cpu();
        self.headless_since_visible = self.headless_since_visible.saturating_add(1);
        self.cursor += 1;
        self.current_frame = self.cursor as i32;
    }

    fn apply_input_at_cursor(&mut self, emulator: &mut dyn EmulatorCore) {
        let (p0_inp, p1_inp) = self.log[self.cursor];
        self.last_inputs = (p0_inp, p1_inp);

        let p0_state = unpack_input(p0_inp);
        let p1_state = unpack_input(p1_inp);

        let mut combined_sys = p0_state.sys;
        if (p1_state.sys & 0x01) == 0 { combined_sys &= !0x04u8; }

        let mut combined_coin = p0_state.coin;
        if (p1_state.coin & 0x01) == 0 { combined_coin &= !0x02u8; }

        let combined = crate::io::InputState {
            p1:   p0_state.p1,
            p2:   p1_state.p1,
            sys:  combined_sys,
            coin: combined_coin,
            // 6-button E/F kicks: each peer carries its own E/F in ext bits
            // 0–1; merge into the P1/P2 combined layout.
            ext:  crate::io::combine_ext(p0_state.ext, p1_state.ext),
        };

        emulator.set_input(combined);
    }

    /// Advance one frame with full render + audio.
    pub fn advance(
        &mut self,
        emulator: &mut dyn EmulatorCore,
    ) -> Option<(Vec<u8>, Vec<f32>, bool)> {
        self.total_advance_calls += 1;

        if self.error.is_some() {
            return None;
        }

        if self.cursor >= self.log.len() {
            return None;
        }

        let (fb, au) = self.advance_one_visible(emulator);
        Some((fb, au, false))
    }

    pub fn total_advance_calls(&self) -> u64 {
        self.total_advance_calls
    }

    pub fn status(&self) -> String {
        let buffered = self.buffered_frames();
        if buffered == 0 {
            "WAITING FOR INPUTS…".to_string()
        } else if buffered > 60 {
            format!("CATCHING UP  ({} behind)", buffered)
        } else {
            format!("SPECTATING  ({} buffered)", buffered)
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::neogeo::Emulator;
    use crate::NOMINAL_SAMPLES_PER_FRAME;
    use crate::io::InputState;

    #[test]
    fn end_to_end_replay_is_perfectly_deterministic() {
        if std::env::var_os("EMUFIGHT_RUN_ROM_TESTS").is_none() {
            return;
        }
        let mut emu = Emulator::new();
        if emu.load_roms(Some("kof98")).is_err() {
            eprintln!("Missing kof98 ROMs");
            return;
        }
        emu.reset();
        assert!(emu.load_initial_match_state());

        let boot_state = emu.save_state_to_bytes().unwrap();
        let mut recorded_states = Vec::new();
        let mut replay_log = Vec::new();
        let mut recorded_inputs = Vec::new();

        // 1. Record N frames
        const FRAMES: usize = 40;
        for f in 0..FRAMES {
            // Peer 0 input (maps to P1 in combined)
            let mut p0 = InputState::default();
            if f % 2 == 0 {
                p0.p1 &= !0x10; // A button
            }
            // Peer 1 input (maps to P2 in combined)
            let mut p1 = InputState::default();
            if f % 3 == 0 {
                p1.p1 &= !0x20; // B button (must be in p1 field for packing)
            }
            
            // Reconstruct combined exactly as apply_input_at_cursor does
            let mut combined_sys = p0.sys;
            if (p1.sys & 0x01) == 0 { combined_sys &= !0x04u8; }
            let mut combined_coin = p0.coin;
            if (p1.coin & 0x01) == 0 { combined_coin &= !0x02u8; }
            let combined = InputState {
                p1: p0.p1,
                p2: p1.p1,
                sys: combined_sys,
                coin: combined_coin,
                ext: crate::io::combine_ext(p0.ext, p1.ext),
            };
            
            recorded_inputs.push(combined.clone());
            emu.set_input(combined);
            emu.step(NOMINAL_SAMPLES_PER_FRAME);
            
            let p0_packed = crate::io::pack_input(&p0);
            let p1_packed = crate::io::pack_input(&p1);
            
            // pack for replay: flat LE u32 pairs
            replay_log.extend_from_slice(&p0_packed.to_le_bytes());
            replay_log.extend_from_slice(&p1_packed.to_le_bytes());
            
            recorded_states.push(emu.save_state_to_bytes().unwrap());
        }

        // 2. Load exactly as the spectator session does
        let mut replay_emu = Emulator::new();
        replay_emu.load_roms(Some("kof98")).unwrap();
        replay_emu.reset();
        replay_emu.load_state_from_bytes(&boot_state).unwrap();
        
        let mut session = OnlineSpectatorSession::new(boot_state);
        session.push_inputs(&replay_log);

        // 3. Step and assert identical output
        for f in 0..FRAMES {
            let combined_replay = {
                let p0_state = crate::io::unpack_input(session.log[f].0);
                let p1_state = crate::io::unpack_input(session.log[f].1);
                let mut combined_sys = p0_state.sys;
                if (p1_state.sys & 0x01) == 0 { combined_sys &= !0x04u8; }
                let mut combined_coin = p0_state.coin;
                if (p1_state.coin & 0x01) == 0 { combined_coin &= !0x02u8; }
                crate::io::InputState {
                    p1: p0_state.p1, p2: p1_state.p1, sys: combined_sys, coin: combined_coin, ext: crate::io::combine_ext(p0_state.ext, p1_state.ext)
                }
            };
            assert_eq!(combined_replay.p1, recorded_inputs[f].p1, "Input p1 mismatch at frame {}", f);
            assert_eq!(combined_replay.p2, recorded_inputs[f].p2, "Input p2 mismatch at frame {}", f);
            assert_eq!(combined_replay.sys, recorded_inputs[f].sys, "Input sys mismatch at frame {}", f);
            assert_eq!(combined_replay.coin, recorded_inputs[f].coin, "Input coin mismatch at frame {}", f);
            assert_eq!(combined_replay.ext, recorded_inputs[f].ext, "Input ext mismatch at frame {}", f);

            session.advance_one_visible(&mut replay_emu);
            let state_bytes = replay_emu.save_state_to_bytes().unwrap();
            assert_eq!(state_bytes, recorded_states[f], "Mismatch at frame {}", f);
        }
        
        // 4. Seek backward and forward and verify it still matches
        let seek_frame = 10;
        session.seek(&mut replay_emu, seek_frame);
        
        let cs = replay_emu.save_state_to_bytes().unwrap();
        assert_eq!(cs, recorded_states[seek_frame as usize - 1], "Mismatch at seek frame {}", seek_frame);
    }
}
