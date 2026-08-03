use crate::EmulatorCore;
use crate::NOMINAL_SAMPLES_PER_FRAME;
use crate::io::PackedInput;
use crate::io::unpack_input;

/// Result of one cooperative [`OnlineSpectatorSession::seek_progress`] slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekProgress {
    /// Still catching up; call again next host tick.
    InProgress { cursor: usize, target: usize },
    /// Target frame is rendered; `cursor` is the new playback position.
    Done { cursor: i32 },
    /// Empty log or boot restore failed.
    Failed,
}

/// Interval between in-memory save-state checkpoints (frames).
/// ~3s at 60 Hz — keeps rewinds short without huge RAM use.
pub const SPECTATOR_CHECKPOINT_INTERVAL: usize = 180;

/// Host-fed spectator / replay session (no GGRS).
///
/// Consumes a stream of packed input pairs from the host (live relay, catch-up
/// blob, or a recorded replay) and drives the local emulator deterministically
/// without contributing input of its own.
///
/// The full input log is retained (never drained). Rewinds restore the nearest
/// prior checkpoint (or boot) and headlessly catch up — never full-render
/// intermediate frames.
pub struct OnlineSpectatorSession {
    /// Append-only full input log (random-access, never drained).
    log: Vec<(PackedInput, PackedInput)>,
    /// Frames consumed so far = index of the next input to apply.
    cursor: usize,
    /// The most recently applied input pair, so the HUD can display exactly
    /// what the emulator advanced.
    last_inputs: (PackedInput, PackedInput),
    /// State at frame 0, used when no checkpoint is available.
    boot_state: Vec<u8>,
    /// Periodic save-states: `(cursor, bytes)` after that many frames applied.
    /// Sorted ascending by cursor; used as rewind anchors.
    checkpoints: Vec<(usize, Vec<u8>)>,
    /// Absolute match frame the session was seeded at (0 unless seeded from a
    /// host checkpoint via `seed_from_checkpoint`). Added to `cursor` by the
    /// public `current_frame()` accessor so late-joined spectators display the
    /// true match frame instead of counting from their own local frame zero.
    base_frame: usize,
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
            checkpoints: Vec::new(),
            base_frame: 0,
            current_frame: 0,
            error: None,
            frames_since_running: 0,
            headless_since_visible: 0,
            total_advance_calls: 0,
        }
    }

    /// Push live frames. Payload is flat little-endian `u32` pairs `(p0, p1)`.
    /// Truncated trailing bytes (< 8) are ignored so partial wire frames never desync the log.
    pub fn push_inputs(&mut self, payload: &[u8]) {
        if payload.len() % 8 != 0 {
            log::warn!(
                "SPECTATE: live payload len {} not multiple of 8 — using complete pairs only",
                payload.len()
            );
        }
        let pairs = decode_input_pairs(payload);
        self.log.extend(pairs);
    }

    /// Push catch-up frames. Payload is the same flat LE `u32` pair format,
    /// typically a single concatenated blob from the host relay or a stored replay.
    /// Soft-fails: keeps complete pairs, logs remainder (never drops the whole blob).
    pub fn push_catchup(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        if payload.len() < 8 {
            log::error!(
                "SPECTATE: catch-up payload too short ({} bytes) — need ≥8 for one frame pair",
                payload.len()
            );
            return;
        }
        if payload.len() % 8 != 0 {
            log::warn!(
                "SPECTATE: catch-up len {} not multiple of 8 — truncating remainder",
                payload.len()
            );
        }
        let pairs = decode_input_pairs(payload);
        let total = pairs.len();
        if total == 0 {
            log::error!("SPECTATE: catch-up decoded 0 frames from {} bytes", payload.len());
            return;
        }
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

    /// Current playback position: the absolute match frame consumed so far
    /// (accounts for `base_frame` when seeded from a host checkpoint).
    pub fn current_frame(&self) -> usize {
        self.base_frame + self.cursor
    }

    /// Seed directly from a host-provided confirmed-state checkpoint, skipping
    /// the from-boot replay for late-joining live spectators. `frame` is the
    /// absolute match frame the checkpoint represents; inputs pushed after
    /// this call are assumed to start immediately following it. Discards any
    /// buffered log/local checkpoints, since they'd be indexed from the old
    /// (boot) origin. Returns `false` if the state fails to load (caller
    /// should fall back to boot + full catch-up).
    pub fn seed_from_checkpoint(
        &mut self,
        emulator: &mut dyn EmulatorCore,
        frame: usize,
        bytes: &[u8],
    ) -> bool {
        if emulator.load_state_from_bytes(bytes).is_err() {
            return false;
        }
        self.boot_state = bytes.to_vec();
        self.log.clear();
        self.checkpoints.clear();
        self.cursor = 0;
        self.current_frame = 0;
        self.base_frame = frame;
        true
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

    /// Blocking seek to display frame `target` (`1..=len`).
    ///
    /// Prefer [`Self::seek_progress`] on UI threads. This loops slices until
    /// done: rewind restores the nearest checkpoint (or boot), headless
    /// catch-up runs, then one visible land frame.
    ///
    /// Returns the new cursor, or -1 if there is nothing to seek.
    pub fn seek(&mut self, emulator: &mut dyn EmulatorCore, target: i32) -> i32 {
        self.seek_with_on_frame(emulator, target, |_| {})
    }

    /// Like [`Self::seek`], but invokes `on_frame` after every applied frame
    /// (headless and visible). Used by shells that rebuild series scores while
    /// seeking through a recorded replay.
    pub fn seek_with_on_frame(
        &mut self,
        emulator: &mut dyn EmulatorCore,
        target: i32,
        mut on_frame: impl FnMut(&dyn EmulatorCore),
    ) -> i32 {
        loop {
            match self.seek_progress(emulator, target, usize::MAX, 0.0, &mut on_frame) {
                SeekProgress::Done { cursor } => return cursor,
                SeekProgress::Failed => return -1,
                SeekProgress::InProgress { .. } => continue,
            }
        }
    }

    /// Cooperative seek: advance toward `target` with a hard time budget.
    ///
    /// - **Forward** seeks continue from the current cursor (no boot reload).
    /// - **Backward** seeks restore the nearest checkpoint (or boot) **once**,
    ///   then return `InProgress` so the host can paint before catch-up.
    /// - Stops after `max_steps` **or** when `max_ms` wall time is exceeded
    ///   (whichever first), so the host can paint and stay responsive.
    /// - `on_frame` runs after each applied frame (for series score rebuild).
    /// - Intermediate frames are **headless** (`step_cpu`); only the land frame
    ///   is fully rendered. No `save_state` during catch-up.
    pub fn seek_progress(
        &mut self,
        emulator: &mut dyn EmulatorCore,
        target: i32,
        max_steps: usize,
        max_ms: f64,
        mut on_frame: impl FnMut(&dyn EmulatorCore),
    ) -> SeekProgress {
        let len = self.log.len();
        if len == 0 {
            return SeekProgress::Failed;
        }
        let target = target.clamp(1, len as i32) as usize;
        // Already sitting on the requested display frame — no work.
        if self.cursor == target {
            return SeekProgress::Done {
                cursor: self.current_frame,
            };
        }
        // Headless frames to consume before the single rendered frame.
        let before = target - 1;

        // Rewind: restore anchor only, then yield. Catch-up is a later slice so
        // load_state never shares a host tick with dozens of step_cpu frames.
        if self.cursor > before {
            if !self.restore_anchor(emulator, before) {
                return SeekProgress::Failed;
            }
            return SeekProgress::InProgress {
                cursor: self.cursor,
                target,
            };
        }

        // Hard step cap — wall clocks are unreliable on some WASM targets, and
        // save_state must never run inside this loop. Keep slices tiny so the
        // main thread always paints (spinner / input).
        let step_cap = if max_ms > 0.0 {
            max_steps.min(12)
        } else {
            max_steps
        };
        let t0 = std::time::Instant::now();
        let budget = if max_ms > 0.0 {
            std::time::Duration::from_secs_f64(max_ms)
        } else {
            std::time::Duration::from_secs(3600)
        };
        let mut steps = 0usize;
        while self.cursor < before && steps < step_cap {
            self.advance_one_cpu(emulator);
            on_frame(emulator);
            steps += 1;
            if max_ms > 0.0 && t0.elapsed() >= budget {
                break;
            }
        }

        if self.cursor < before {
            return SeekProgress::InProgress {
                cursor: self.cursor,
                target,
            };
        }

        // Land on the target with one visible frame (only render in the whole seek).
        // Skip checkpointing the land frame — seek is hot path; CPs come from stream play.
        if self.cursor < self.log.len() && self.cursor == before {
            let _ = self.advance_one_visible_no_checkpoint(emulator);
            on_frame(emulator);
        }
        SeekProgress::Done {
            cursor: self.current_frame,
        }
    }

    /// Load the best checkpoint with `cursor <= max_cursor`, or boot if none.
    /// Returns false if restore fails.
    ///
    /// Does **not** call `reset()` — NeoGeo `reset` clones SM1 ROM into the Z80
    /// and is unnecessary; `load_state` fully restores mutable state.
    fn restore_anchor(&mut self, emulator: &mut dyn EmulatorCore, max_cursor: usize) -> bool {
        let (anchor_cursor, bytes) = self
            .checkpoints
            .iter()
            .rev()
            .find(|(c, _)| *c <= max_cursor)
            .map(|(c, b)| (*c, b.as_slice()))
            .unwrap_or((0, self.boot_state.as_slice()));

        let ok = if emulator.load_state_from_bytes(bytes).is_ok() {
            self.cursor = anchor_cursor;
            self.current_frame = anchor_cursor as i32;
            true
        } else if anchor_cursor != 0
            && emulator.load_state_from_bytes(&self.boot_state).is_ok()
        {
            // Corrupt checkpoint — fall back to boot.
            self.cursor = 0;
            self.current_frame = 0;
            true
        } else {
            false
        };
        self.headless_since_visible = 0;
        ok
    }

    /// Snapshot emu after `cursor` frames if on a checkpoint boundary.
    fn maybe_checkpoint(&mut self, emulator: &mut dyn EmulatorCore) {
        let c = self.cursor;
        if c == 0 || c % SPECTATOR_CHECKPOINT_INTERVAL != 0 {
            return;
        }
        if self.checkpoints.last().map(|(lc, _)| *lc) == Some(c) {
            return;
        }
        if let Ok(bytes) = emulator.save_state_to_bytes() {
            self.checkpoints.push((c, bytes));
        }
    }

    /// Nearest checkpoint cursor ≤ `max_cursor` (for series restore in the shell).
    pub fn best_checkpoint_cursor(&self, max_cursor: usize) -> Option<usize> {
        self.checkpoints
            .iter()
            .rev()
            .find(|(c, _)| *c <= max_cursor)
            .map(|(c, _)| *c)
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

    fn advance_one_visible(&mut self, emulator: &mut dyn EmulatorCore) -> (Vec<u8>, Vec<f32>) {
        let out = self.advance_one_visible_no_checkpoint(emulator);
        // Checkpoints only while streaming play — never during rewind catch-up
        // (save_state does a load round-trip and freezes seeks).
        self.maybe_checkpoint(emulator);
        out
    }

    /// Visible step without save-state (seek land frame).
    fn advance_one_visible_no_checkpoint(
        &mut self,
        emulator: &mut dyn EmulatorCore,
    ) -> (Vec<u8>, Vec<f32>) {
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
        // No checkpoint here — headless path is for seek/catch-up speed.
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
